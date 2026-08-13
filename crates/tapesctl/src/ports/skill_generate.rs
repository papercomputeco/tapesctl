//! `tapesctl skill generate` — ported from `tapes skill generate`.
//!
//! The pipeline is unchanged: resolve sessions, render each as a transcript,
//! ask an LLM to extract a reusable skill, write the result to
//! `~/.tapes/skills/<name>.md`.
//!
//! # The one real difference: no serverless mode
//!
//! The Go command could run with **no server at all**. Given `--postgres` it
//! started an in-process API against that database and pointed its own client
//! at it, so `skill generate` worked on a machine with a tapes database and no
//! tapes process. tapesctl is an HTTP client by construction — it has no
//! embedded server and no database driver — so that path is gone and
//! `--postgres` is not reproduced. Everything the flag enabled is still
//! reachable by running `tapes serve api` and pointing `--tapes-url` at it.
//!
//! Nothing else needed the database: the Go generator already read its
//! transcripts through the same two HTTP routes this port calls, so the
//! extraction itself is unchanged rather than approximated.
//!
//! # Smaller drops
//!
//! The Go command asked the model to *suggest* a name when `--name` was empty.
//! That branch was unreachable — the flag is required in both implementations —
//! so the prompt here always pins the caller's name. Progress was rendered as
//! spinners and the finished skill through a markdown renderer; tapesctl prints
//! the same step text plainly and the same markdown unstyled, which is the
//! fallback path the Go command already took when its renderer failed.

use std::path::{Path, PathBuf};

use snafu::OptionExt;
use tapes_client::core::models::SearchSpansParams;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;

use crate::api::client::{ApiClient, narrow};
use crate::api::resolve_client;
use crate::cli::SkillGenerateArgs;
use crate::error::{Result, error};
use crate::ports::search::session_ids;
use crate::ports::skill_document::{self, INITIAL_VERSION, SKILL_TYPES, Skill};
use crate::ports::skill_llm::{LlmCaller, LlmConfig};
use crate::ports::skill_paths::validate_name;
use crate::ports::skill_transcript::{TurnFilter, build_session_transcript};

/// Prompt ceiling. Transcripts are dropped at a session boundary once the
/// combined length would exceed this.
const MAX_TRANSCRIPT_CHARS: usize = 30_000;

/// Attempts to get parseable JSON out of the model.
const MAX_PARSE_ATTEMPTS: u32 = 3;

/// Run one `skill generate`.
pub async fn run(args: SkillGenerateArgs) -> Result<()> {
    validate_name(&args.name)?;
    snafu::ensure!(
        skill_document::valid_skill_type(&args.skill_type),
        error::InvalidSkillTypeSnafu {
            value: args.skill_type.clone(),
            valid: SKILL_TYPES.join(", "),
        }
    );
    let filter = turn_filter(args.since.as_deref(), args.until.as_deref())?;

    println!("tapesctl: Connecting to API");
    let client = resolve_client(&args.api)?;
    let sessions = resolve_sessions(&client, &args).await?;

    println!("tapesctl: Configuring LLM provider");
    let caller = LlmCaller::new(&LlmConfig {
        provider: args.provider.clone(),
        model: args.model.clone(),
        api_key: args.api_key.clone(),
        base_url: None,
    })?;

    println!(
        "\nGenerating skill {:?} from {} session(s) via {} {}\n",
        args.name,
        sessions.len(),
        caller.provider().as_str(),
        caller.model(),
    );

    println!("tapesctl: Extracting skill from session transcript(s)");
    let transcript = combined_transcript(&client, &sessions, &filter).await?;
    let skill = extract(
        &caller,
        &transcript,
        &args.name,
        &args.skill_type,
        &sessions,
    )
    .await?;

    // The document itself, so a `--preview` run shows exactly what would land.
    println!("\n{}", skill_document::render(&skill));

    if args.preview {
        return Ok(());
    }

    println!("tapesctl: Writing SKILL.md");
    let (dir, base) = authoring_dir(args.source_dir)?;
    let written = skill_document::write(&skill, &dir, &base)?;
    println!("\n  Saved to {}", written.display());
    println!(
        "  Run 'tapesctl skill sync {}' to install it for an agent\n",
        skill.name,
    );
    Ok(())
}

/// Where the skill is written, and the tree that write must stay inside.
///
/// With no `--source-dir` the base is the home directory, so a symlinked
/// `~/.tapes/skills` is refused. When the user names a directory explicitly it
/// is its own base: they pointed at that path deliberately, so resolving it is
/// their instruction — but a symlinked file *inside* it is still refused.
fn authoring_dir(source_dir: Option<PathBuf>) -> Result<(PathBuf, PathBuf)> {
    match source_dir {
        Some(dir) => Ok((dir.clone(), dir)),
        None => {
            let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
            Ok((skill_document::skills_dir(&home), home))
        }
    }
}

/// Positional session ids, else `--search`.
async fn resolve_sessions(client: &ApiClient, args: &SkillGenerateArgs) -> Result<Vec<String>> {
    if !args.session_ids.is_empty() {
        return Ok(args.session_ids.clone());
    }
    let query = args
        .search
        .as_deref()
        .context(error::NoSessionsNamedSnafu)?;

    println!("Searching for {query:?}...");
    let output = client
        .search_spans(&SearchSpansParams {
            query: query.to_owned(),
            top_k: Some(narrow(args.search_top)),
        })
        .await?;

    let sessions = session_ids(&output.results);
    snafu::ensure!(
        !sessions.is_empty(),
        error::NoSearchResultsSnafu {
            query: query.to_owned(),
        }
    );
    for session in &sessions {
        println!("  found: {session}");
    }
    Ok(sessions)
}

/// Render every session, dropping whole sessions once the prompt would be
/// oversized.
///
/// The first session is always kept even if it alone exceeds the ceiling:
/// trimming to nothing would spend an LLM call on an empty prompt, and a single
/// long transcript still fits a modern context window.
async fn combined_transcript(
    client: &ApiClient,
    sessions: &[String],
    filter: &TurnFilter,
) -> Result<String> {
    let mut rendered: Vec<String> = Vec::with_capacity(sessions.len());
    for session in sessions {
        rendered.push(build_session_transcript(client, session, filter).await?);
    }

    let mut total = 0usize;
    for (index, transcript) in rendered.iter().enumerate() {
        total = total.saturating_add(transcript.len());
        if index > 0 {
            total = total.saturating_add("\n---\n".len());
        }
        if index > 0 && total > MAX_TRANSCRIPT_CHARS {
            eprintln!(
                "tapesctl: transcript truncated to {index} of {} session(s) to fit within the {MAX_TRANSCRIPT_CHARS} char limit",
                sessions.len(),
            );
            rendered.truncate(index);
            break;
        }
    }
    Ok(rendered.join("\n---\n"))
}

/// Ask the model for a skill, retrying a response that will not parse.
///
/// A transport or provider failure aborts immediately — retrying a bad key or a
/// missing model just spends the same error again. Only an unparseable
/// *response* is retried, with a blunter instruction each time.
async fn extract(
    caller: &LlmCaller,
    transcript: &str,
    name: &str,
    skill_type: &str,
    sessions: &[String],
) -> Result<Skill> {
    let base_prompt = build_prompt(transcript, name, skill_type);
    let mut last: Option<crate::Error> = None;

    for attempt in 0..MAX_PARSE_ATTEMPTS {
        let prompt = if attempt == 0 {
            base_prompt.clone()
        } else {
            format!("{base_prompt}\n\nReturn ONLY valid JSON, no markdown.")
        };

        let response = caller.call(&prompt).await?;
        match parse_response(&response) {
            Ok(mut skill) => {
                // Caller-supplied facts win over anything the model said about
                // them: the name addresses the file, and the rest is provenance
                // the model is in no position to know.
                skill.name = name.to_owned();
                skill.skill_type = skill_type.to_owned();
                skill.sessions = sessions.to_vec();
                skill.version = INITIAL_VERSION.to_owned();
                skill.created_at = Some(OffsetDateTime::now_utc());
                return Ok(skill);
            }
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| {
        error::SkillNotExtractedSnafu {
            attempts: MAX_PARSE_ATTEMPTS,
        }
        .build()
    }))
}

/// Pull the JSON object out of a model response and read it as a skill.
///
/// Models wrap JSON in prose or fences often enough that the Go command
/// narrowed to the outermost braces before parsing; that salvage is reproduced
/// because without it a well-formed skill inside a code fence is thrown away.
fn parse_response(response: &str) -> Result<Skill> {
    let document = match (response.find('{'), response.rfind('}')) {
        (Some(start), Some(end)) if end > start => &response[start..=end],
        _ => response,
    };
    serde_json::from_str(document).map_err(|source| crate::Error::SkillJson { source })
}

/// Build the extraction prompt.
fn build_prompt(transcript: &str, name: &str, skill_type: &str) -> String {
    format!(
        r#"Analyze the following LLM coding session transcript(s) and extract a reusable skill.

The skill should be named {name:?} and categorized as {skill_type:?}.

Transcript format: [user] lines are the human's prompts, [assistant]
lines are the agent's responses, and [tools] lines summarize the tools
the agent invoked between responses.

Return ONLY valid JSON with these fields:

{{
  "description": "A clear description with trigger phrases for when an agent should use this skill. Start with an action verb.",
  "tags": ["array", "of", "relevant", "tags"],
  "content": "Markdown body with step-by-step instructions in imperative form. Use ## headers and numbered steps."
}}

Guidelines for extraction:
- Identify the reusable pattern/workflow from the session(s)
- Write a clear description with trigger phrases (e.g. "Use when debugging React hooks issues")
- Write step-by-step instructions in imperative form
- Focus on the generalizable technique, not session-specific details
- Use the [tools] lines to capture which tools the workflow relies on
- Include any important caveats or edge cases observed

Transcript(s):
{transcript}"#,
    )
}

/// Resolve `--since` / `--until` into a turn window.
fn turn_filter(since: Option<&str>, until: Option<&str>) -> Result<TurnFilter> {
    Ok(TurnFilter {
        since: parse_bound("--since", since)?,
        until: parse_bound("--until", until)?,
    })
}

/// Accept RFC 3339 or a bare `YYYY-MM-DD`.
///
/// A bare date names the whole day: as a lower bound it starts at midnight
/// UTC, as an upper bound it ends at the day's last instant — otherwise
/// `--until 2026-07-31` would exclude everything after the day's first
/// nanosecond, silently dropping nearly the entire final day the user asked
/// for.
fn parse_bound(flag: &'static str, value: Option<&str>) -> Result<Option<OffsetDateTime>> {
    let Some(raw) = value.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    if let Ok(parsed) = OffsetDateTime::parse(raw, &Rfc3339) {
        return Ok(Some(parsed));
    }
    if let Ok(date) = time::Date::parse(raw, format_description!("[year]-[month]-[day]")) {
        let start = date.midnight().assume_utc();
        return Ok(Some(if flag == "--until" {
            start + time::Duration::days(1) - time::Duration::nanoseconds(1)
        } else {
            start
        }));
    }
    error::InvalidSkillTimeSnafu {
        flag,
        value: raw.to_owned(),
    }
    .fail()
}

/// Where authored skills live, exposed for the list command.
#[must_use]
pub fn default_authoring_dir(home: &Path) -> PathBuf {
    skill_document::skills_dir(home)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::ApiArgs;
    use serde_json::json;
    use url::Url;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args(server: &MockServer) -> SkillGenerateArgs {
        SkillGenerateArgs {
            api: ApiArgs {
                tapes_url: Some(server.uri()),
            },
            session_ids: vec!["s-1".to_owned()],
            name: "debug-hooks".to_owned(),
            skill_type: "workflow".to_owned(),
            preview: false,
            provider: "openai".to_owned(),
            model: None,
            api_key: Some("k".to_owned()),
            since: None,
            until: None,
            search: None,
            search_top: 3,
            // Never `None`, which would make [`authoring_dir`] answer with the
            // developer's real `~/.tapes/skills`. Every test using this
            // fixture stops before the write — behind a live model call — so
            // nothing reaches it today, and that is exactly the kind of
            // protection that lapses silently the moment someone stubs the
            // model out. A path that cannot be created fails such a test
            // loudly instead of writing to the runner's home; a test that
            // means to assert the write should name its own temporary
            // directory here.
            source_dir: Some(PathBuf::from("/nonexistent/tapesctl-test-skills")),
        }
    }

    fn client_for(server: &MockServer) -> ApiClient {
        crate::api::client::connect(Url::parse(&server.uri()).unwrap())
    }

    #[test]
    fn a_date_bound_means_midnight_utc() {
        let at = parse_bound("--since", Some("2026-07-31")).unwrap().unwrap();
        assert_eq!(at.to_string(), "2026-07-31 0:00:00.0 +00:00:00");
        // A bare upper bound covers the WHOLE named day.
        let at = parse_bound("--until", Some("2026-07-31")).unwrap().unwrap();
        assert_eq!(at.to_string(), "2026-07-31 23:59:59.999999999 +00:00:00");
    }

    #[test]
    fn an_rfc3339_bound_is_taken_as_given() {
        let at = parse_bound("--until", Some("2026-07-31T12:34:56Z"))
            .unwrap()
            .unwrap();
        assert_eq!(at.hour(), 12);
        assert_eq!(at.second(), 56);
    }

    #[test]
    fn an_unparseable_bound_names_the_flag_and_the_value() {
        let err = parse_bound("--since", Some("last tuesday")).unwrap_err();
        let rendered = format!("{err}");
        assert!(rendered.contains("--since"), "got: {rendered}");
        assert!(rendered.contains("last tuesday"), "got: {rendered}");
    }

    #[test]
    fn an_absent_bound_is_no_bound() {
        assert!(parse_bound("--since", None).unwrap().is_none());
        assert!(parse_bound("--since", Some("  ")).unwrap().is_none());
    }

    #[test]
    fn the_prompt_pins_the_callers_name_and_type() {
        let prompt = build_prompt("[user] hi\n", "debug-hooks", "workflow");
        assert!(
            prompt.contains(r#"named "debug-hooks" and categorized as "workflow""#),
            "got: {prompt}",
        );
        assert!(prompt.ends_with("[user] hi\n"), "transcript goes last");
        assert!(
            !prompt.contains("suggest a concise"),
            "the name-suggestion branch is unreachable and not reproduced",
        );
    }

    #[test]
    fn json_wrapped_in_prose_or_fences_is_still_read() {
        // Models do this constantly; discarding the skill would be a wasted
        // call and a confusing error.
        let skill = parse_response(
            "Here you go!\n```json\n{\"description\":\"d\",\"tags\":[\"a\"],\"content\":\"c\"}\n```\nHope that helps.",
        )
        .unwrap();
        assert_eq!(skill.description, "d");
        assert_eq!(skill.tags, vec!["a"]);
    }

    #[test]
    fn a_response_with_no_json_is_an_error() {
        assert!(parse_response("I could not do that.").is_err());
    }

    #[test]
    fn a_model_omitting_optional_fields_still_yields_a_skill() {
        let skill = parse_response(r#"{"description":"just this"}"#).unwrap();
        assert_eq!(skill.description, "just this");
        assert!(skill.tags.is_empty());
    }

    #[tokio::test]
    async fn an_invalid_type_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let mut args = args(&server);
        args.skill_type = "encyclopedia".to_owned();

        let err = run(args).await.unwrap_err();

        assert!(format!("{err}").contains("encyclopedia"), "got: {err}");
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_traversal_name_is_refused_before_any_request() {
        let server = MockServer::start().await;
        let mut args = args(&server);
        args.name = "../escape".to_owned();

        let err = run(args).await.unwrap_err();

        assert!(
            format!("{err}").contains("invalid skill name"),
            "got: {err}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn naming_no_sessions_and_no_search_is_an_error() {
        let server = MockServer::start().await;
        let mut args = args(&server);
        args.session_ids.clear();

        let err = run(args).await.unwrap_err();
        assert!(format!("{err}").contains("--search"), "got: {err}");
    }

    #[tokio::test]
    async fn search_resolves_sessions_deduplicated_in_score_order() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search/spans"))
            .and(query_param("query", "react hooks"))
            .and(query_param("top_k", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"session_id": "s-a"},
                {"session_id": "s-a"},
                {"session_id": "s-b"},
            ]})))
            .mount(&server)
            .await;

        let mut args = args(&server);
        args.session_ids.clear();
        args.search = Some("react hooks".to_owned());
        args.search_top = 2;

        let resolved = resolve_sessions(&client_for(&server), &args).await.unwrap();
        assert_eq!(resolved, vec!["s-a", "s-b"]);
    }

    #[tokio::test]
    async fn a_search_matching_nothing_names_the_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search/spans"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;

        let mut args = args(&server);
        args.session_ids.clear();
        args.search = Some("nothing at all".to_owned());

        let err = resolve_sessions(&client_for(&server), &args)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("nothing at all"), "got: {err}");
    }

    #[tokio::test]
    async fn positional_sessions_win_over_a_search_query() {
        // The Go resolution order, and the reason a stale --search in a shell
        // history cannot silently redirect an explicit request.
        let server = MockServer::start().await;
        let mut args = args(&server);
        args.search = Some("ignored".to_owned());

        let resolved = resolve_sessions(&client_for(&server), &args).await.unwrap();

        assert_eq!(resolved, vec!["s-1"]);
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no search should be issued",
        );
    }

    #[tokio::test]
    async fn an_oversized_prompt_drops_whole_sessions_but_never_the_first() {
        let server = MockServer::start().await;
        let long = "x".repeat(MAX_TRANSCRIPT_CHARS + 100);
        Mock::given(method("GET"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [{
                "trace_id": "t-1",
                "user_prompt": long,
            }]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"spans": []})))
            .mount(&server)
            .await;

        let sessions = vec!["s-1".to_owned(), "s-2".to_owned(), "s-3".to_owned()];
        let combined = combined_transcript(&client_for(&server), &sessions, &TurnFilter::default())
            .await
            .unwrap();

        assert!(
            !combined.contains("\n---\n"),
            "only the first session should survive",
        );
        assert!(!combined.is_empty(), "the prompt must not be emptied");
    }
}
