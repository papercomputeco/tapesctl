//! Reducing a cassette's OpenAPI document to the surface a CLI needs.
//!
//! This is deliberately *not* an OpenAPI implementation. A command line needs
//! five things from an operation — what to call it, which verb, which path,
//! which inputs, and whether it takes a body — and everything else in the
//! document (response schemas, examples, security, servers) describes what comes
//! *back*, which [`crate::api::client`] prints verbatim without modelling. So
//! the reducer reads the handful of keys it acts on and ignores the rest, which
//! also means a document using a feature this build predates still yields a
//! working command instead of an error.
//!
//! # Naming
//!
//! A method's name is its `operationId`, kebab-cased: `getHello` becomes
//! `tapesctl hello-world get-hello`. The id is the one name in the document the
//! cassette author chose *for the operation itself* rather than for its
//! transport, it is unique within a document by the OpenAPI spec, and core
//! leaves it bare in the per-cassette document (only the merged aggregate
//! namespaces ids). An operation with no id gets one synthesized from its verb
//! and path, which is what core would have done anyway.

use std::collections::BTreeSet;

use serde_json::Value;

/// The HTTP verbs OpenAPI defines as operations on a path item.
///
/// A path item also holds `summary`, `description`, `servers`, `parameters` and
/// `$ref`, so "a key under a path" and "a method" are not the same thing — a
/// reader that assumed they were would publish the shared parameter list as a
/// command.
const HTTP_METHODS: [&str; 8] = [
    "get", "put", "post", "delete", "patch", "head", "options", "trace",
];

/// Flag names the generated surface cannot hand to a cassette parameter,
/// because the cassette subcommand defines them itself.
const RESERVED_FLAGS: [&str; 4] = ["tapes-url", "body", "help", "verbose"];

/// Where a parameter travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    /// Substituted into the path template; becomes a positional argument.
    Path,
    /// Appended to the query string; becomes a `--flag`.
    Query,
    /// Sent as a request header; becomes a `--flag`.
    Header,
}

/// One input to a generated method.
#[derive(Debug, Clone)]
pub struct Param {
    /// The name on the wire, used verbatim in the query string or header.
    pub wire: String,
    /// The long-flag or positional name presented to the user.
    pub flag: String,
    /// Where it travels.
    pub location: Location,
    /// Whether the operation requires it.
    pub required: bool,
    /// Help text, when the document carries any.
    pub description: Option<String>,
}

/// One generated method.
#[derive(Debug, Clone)]
pub struct Method {
    /// The subcommand name.
    pub name: String,
    /// One line of help.
    pub summary: Option<String>,
    /// The HTTP verb, uppercased.
    pub http_method: String,
    /// The public path template, used verbatim — core already republished the
    /// document onto the paths a client can call.
    pub path: String,
    /// Path, query and header inputs.
    pub params: Vec<Param>,
    /// `Some(true)` when a request body is required, `Some(false)` when it is
    /// accepted but optional, `None` when the operation takes none.
    pub body: Option<bool>,
}

impl Method {
    /// The path parameters, in the order they appear in the path template.
    #[must_use]
    pub fn path_params(&self) -> Vec<&Param> {
        self.params
            .iter()
            .filter(|param| param.location == Location::Path)
            .collect()
    }
}

/// One cassette's generated surface.
#[derive(Debug, Clone)]
pub struct Cassette {
    /// The noun on the command line.
    pub name: String,
    /// One line of help.
    pub description: Option<String>,
    /// Its methods, ordered by name.
    pub methods: Vec<Method>,
}

/// Every cassette a server serves.
#[derive(Debug, Clone, Default)]
pub struct Surface {
    /// The cassettes, ordered by name.
    pub cassettes: Vec<Cassette>,
}

impl Surface {
    /// Whether there is anything to generate.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cassettes.is_empty()
    }

    /// Find a cassette by its noun.
    #[must_use]
    pub fn cassette(&self, name: &str) -> Option<&Cassette> {
        self.cassettes.iter().find(|c| c.name == name)
    }
}

/// Reduce one cassette's OpenAPI document to its methods.
///
/// Never fails: an operation the reducer cannot make sense of is dropped, and a
/// document with no usable operations yields a cassette with no methods. A hard
/// error here would take out the whole CLI over one malformed cassette.
#[must_use]
pub fn reduce(entry_name: &str, description: Option<String>, document: &Value) -> Cassette {
    let mut methods: Vec<Method> = Vec::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();

    if let Some(paths) = document.get("paths").and_then(Value::as_object) {
        // serde_json orders object keys, so the generated surface is stable
        // between invocations against an unchanged document.
        for (path, item) in paths {
            methods.extend(methods_of(path, item, document, &mut taken));
        }
    }

    methods.sort_by(|a, b| a.name.cmp(&b.name));

    Cassette {
        name: entry_name.to_owned(),
        description,
        methods,
    }
}

/// Every operation on one path item.
///
/// `taken` is threaded through rather than scoped per path because method names
/// have to be unique across the whole cassette, not just within one path.
fn methods_of(
    path: &str,
    item: &Value,
    document: &Value,
    taken: &mut BTreeSet<String>,
) -> Vec<Method> {
    let Some(item) = item.as_object() else {
        return Vec::new();
    };
    let shared = parameters_of(item.get("parameters"), document);

    HTTP_METHODS
        .iter()
        .filter_map(|verb| {
            let operation = item.get(*verb)?.as_object()?;

            let mut params = shared.clone();
            params.extend(parameters_of(operation.get("parameters"), document));

            let raw_name = operation
                .get("operationId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map_or_else(|| synthesize_id(verb, path), kebab_case);

            Some(Method {
                name: unique(raw_name, verb, taken),
                summary: text_of(operation.get("summary"))
                    .or_else(|| text_of(operation.get("description"))),
                http_method: verb.to_ascii_uppercase(),
                path: path.to_owned(),
                params: finish_params(path, params),
                body: operation.get("requestBody").map(body_required),
            })
        })
        .collect()
}

/// Whether a request body object marks itself required. Absent means optional,
/// which is what OpenAPI's own default says.
fn body_required(body: &Value) -> bool {
    body.get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Read a parameter list, resolving local `$ref`s into
/// `#/components/parameters`.
///
/// A reference that does not resolve is dropped rather than guessed at: a
/// parameter whose name is unknown cannot be sent under the right name, and
/// inventing one would produce a request the server rejects for a reason the
/// user cannot see.
fn parameters_of(value: Option<&Value>, document: &Value) -> Vec<Param> {
    let Some(list) = value.and_then(Value::as_array) else {
        return Vec::new();
    };

    list.iter()
        .filter_map(|entry| {
            let resolved = match entry.get("$ref").and_then(Value::as_str) {
                Some(reference) => resolve(reference, document)?,
                None => entry,
            };
            parameter(resolved)
        })
        .collect()
}

/// Resolve a local JSON pointer of the `#/a/b/c` form.
fn resolve<'a>(reference: &str, document: &'a Value) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    document.pointer(pointer)
}

/// Read one parameter object.
fn parameter(value: &Value) -> Option<Param> {
    let wire = value.get("name").and_then(Value::as_str)?.trim();
    if wire.is_empty() {
        return None;
    }
    let location = match value.get("in").and_then(Value::as_str) {
        Some("path") => Location::Path,
        Some("query") => Location::Query,
        Some("header") => Location::Header,
        // `cookie` is the only other location OpenAPI defines, and a CLI has no
        // sensible way to offer it. Anything else is not a parameter at all.
        _ => return None,
    };

    Some(Param {
        wire: wire.to_owned(),
        flag: kebab_case(wire),
        location,
        // A path parameter is required by definition; OpenAPI says so and a
        // document that claims otherwise still cannot produce a callable URL.
        required: location == Location::Path
            || value
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        description: text_of(value.get("description")),
    })
}

/// Put a parameter list into its final shape: path parameters in template
/// order, then the rest, with every flag name unique and none of them colliding
/// with a flag the subcommand defines itself.
fn finish_params(path: &str, params: Vec<Param>) -> Vec<Param> {
    let templated = template_params(path);

    let mut ordered: Vec<Param> = Vec::new();
    // Template order wins for positionals: the URL is built by substitution, so
    // the order the user types them in has to be the order they appear.
    for name in &templated {
        if let Some(found) = params
            .iter()
            .find(|p| p.location == Location::Path && &p.wire == name)
        {
            ordered.push(found.clone());
        } else {
            // Declared in the path but not in `parameters`. The URL cannot be
            // built without a value for it, so synthesize the input rather than
            // generate a command that can only produce a broken request.
            ordered.push(Param {
                wire: name.clone(),
                flag: kebab_case(name),
                location: Location::Path,
                required: true,
                description: None,
            });
        }
    }
    for param in params {
        // A declared path parameter the template never mentions has nowhere to
        // go; keeping it would offer an argument that changes nothing.
        if param.location != Location::Path {
            ordered.push(param);
        }
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for param in &mut ordered {
        if RESERVED_FLAGS.contains(&param.flag.as_str()) {
            param.flag = format!("param-{}", param.flag);
        }
        let mut candidate = param.flag.clone();
        let mut suffix = 2;
        while !seen.insert(candidate.clone()) {
            candidate = format!("{}-{suffix}", param.flag);
            suffix += 1;
        }
        param.flag = candidate;
    }

    ordered
}

/// The `{name}` placeholders in a path template, in order.
fn template_params(path: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let name = &rest[open + 1..open + close];
        if !name.is_empty() {
            found.push(name.to_owned());
        }
        rest = &rest[open + close + 1..];
    }
    found
}

/// Derive a method name from a verb and a path, for an operation with no
/// `operationId`.
///
/// The cassette's own prefix is not stripped: this runs on the republished
/// document, where every path starts `/v1/cassettes/<name>/`, and those segments
/// are dropped so the name describes the operation rather than repeating the
/// noun it already sits under.
fn synthesize_id(verb: &str, path: &str) -> String {
    let mut parts = vec![verb.to_ascii_lowercase()];
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // `/v1/cassettes/<name>/rest...` — drop the three-segment mount point.
    let tail = if segments.len() > 3 && segments[0] == "v1" && segments[1] == "cassettes" {
        &segments[3..]
    } else {
        &segments[..]
    };
    for segment in tail {
        let cleaned: String = segment
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !cleaned.is_empty() {
            parts.push(kebab_case(&cleaned));
        }
    }
    if parts.len() == 1 {
        // Nothing but the mount point; the verb alone is still a usable name.
        return parts.remove(0);
    }
    parts.join("-")
}

/// Make a name unique within one cassette.
///
/// Operation ids are unique within an OpenAPI document by the specification, so
/// this only fires when two ids kebab to the same thing (`getHello` and
/// `get_hello`). The verb disambiguates first because it is meaningful; a
/// counter is the last resort.
fn unique(name: String, verb: &str, taken: &mut BTreeSet<String>) -> String {
    if taken.insert(name.clone()) {
        return name;
    }
    let with_verb = format!("{name}-{verb}");
    if taken.insert(with_verb.clone()) {
        return with_verb;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{name}-{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        suffix += 1;
    }
}

/// A trimmed, non-empty string, or nothing.
fn text_of(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Convert an identifier to kebab-case.
///
/// `getHello` → `get-hello`, `since_id` → `since-id`, `getHTTPStatus` →
/// `get-http-status`: a run of capitals stays together, and only the last one
/// starts the next word, so an acronym does not explode into single letters.
fn kebab_case(raw: &str) -> String {
    let chars: Vec<char> = raw.trim().chars().collect();
    let mut out = String::with_capacity(chars.len() + 4);

    for (index, &current) in chars.iter().enumerate() {
        if current == '_' || current == ' ' || current == '.' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            continue;
        }
        if current == '-' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            continue;
        }
        if current.is_ascii_uppercase() && index > 0 {
            let previous = chars[index - 1];
            let starts_word = previous.is_ascii_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_ascii_uppercase()
                    && chars.get(index + 1).is_some_and(char::is_ascii_lowercase));
            if starts_word && !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
        }
        out.extend(current.to_lowercase());
    }

    out.trim_matches('-').to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hello_world() -> Value {
        // The shape core republishes: paths already public, ids still bare.
        json!({
            "openapi": "3.1.0",
            "paths": {
                "/v1/cassettes/hello-world/hello": {
                    "get": {
                        "operationId": "getHello",
                        "summary": "Greet, and read back every stored row"
                    },
                    "post": {
                        "operationId": "createHello",
                        "summary": "Write one row to the hello table",
                        "requestBody": {"required": false}
                    }
                }
            }
        })
    }

    #[test]
    fn an_operation_id_becomes_a_kebab_case_method() {
        let cassette = reduce("hello-world", None, &hello_world());
        let names: Vec<&str> = cassette.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["create-hello", "get-hello"]);
    }

    #[test]
    fn the_republished_path_is_used_verbatim() {
        // Core already rewrote the cassette's own `/api/<name>/hello` onto the
        // public surface; rewriting it again here would break the request.
        let cassette = reduce("hello-world", None, &hello_world());
        let method = cassette
            .methods
            .iter()
            .find(|m| m.name == "get-hello")
            .unwrap();
        assert_eq!(method.path, "/v1/cassettes/hello-world/hello");
        assert_eq!(method.http_method, "GET");
    }

    #[test]
    fn an_optional_request_body_is_distinguished_from_a_required_one_and_from_none() {
        let document = json!({"paths": {"/v1/cassettes/c/thing": {
            "post": {"operationId": "a", "requestBody": {"required": true}},
            "put": {"operationId": "b", "requestBody": {}},
            "get": {"operationId": "c"}
        }}});
        let cassette = reduce("c", None, &document);
        let body = |name: &str| {
            cassette
                .methods
                .iter()
                .find(|m| m.name == name)
                .unwrap()
                .body
        };
        assert_eq!(body("a"), Some(true));
        assert_eq!(body("b"), Some(false));
        assert_eq!(body("c"), None);
    }

    #[test]
    fn path_parameters_are_ordered_by_the_template_not_by_the_declaration() {
        // The URL is built by substitution, so positional order must follow the
        // path; a document is free to declare them in any order.
        let document = json!({"paths": {"/v1/cassettes/c/{owner}/reports/{id}": {
            "parameters": [
                {"name": "id", "in": "path", "required": true},
                {"name": "owner", "in": "path", "required": true}
            ],
            "get": {"operationId": "getReport"}
        }}});
        let cassette = reduce("c", None, &document);
        let method = &cassette.methods[0];
        let names: Vec<&str> = method
            .path_params()
            .iter()
            .map(|p| p.wire.as_str())
            .collect();
        assert_eq!(names, vec!["owner", "id"]);
    }

    #[test]
    fn a_templated_segment_with_no_declaration_still_becomes_an_argument() {
        // Without a value for it there is no callable URL, so generating the
        // command without the argument would only produce broken requests.
        let document = json!({"paths": {"/v1/cassettes/c/reports/{id}": {
            "get": {"operationId": "getReport"}
        }}});
        let cassette = reduce("c", None, &document);
        assert_eq!(cassette.methods[0].path_params()[0].wire, "id");
        assert!(cassette.methods[0].path_params()[0].required);
    }

    #[test]
    fn shared_path_item_parameters_reach_every_operation() {
        let document = json!({"paths": {"/v1/cassettes/c/reports": {
            "parameters": [{"name": "since", "in": "query"}],
            "get": {"operationId": "listReports"},
            "post": {"operationId": "createReport"}
        }}});
        let cassette = reduce("c", None, &document);
        for method in &cassette.methods {
            assert!(
                method.params.iter().any(|p| p.wire == "since"),
                "{} lost the shared parameter",
                method.name,
            );
        }
    }

    #[test]
    fn a_shared_parameter_list_is_not_mistaken_for_an_operation() {
        // `parameters` is a path-item key but not a method; a reader that
        // treated every key as an operation would publish it as a command.
        let document = json!({"paths": {"/v1/cassettes/c/reports": {
            "parameters": [{"name": "since", "in": "query"}],
            "summary": "not an operation",
            "get": {"operationId": "listReports"}
        }}});
        let cassette = reduce("c", None, &document);
        assert_eq!(cassette.methods.len(), 1);
        assert_eq!(cassette.methods[0].name, "list-reports");
    }

    #[test]
    fn a_referenced_parameter_is_resolved_from_components() {
        let document = json!({
            "components": {"parameters": {"Since": {"name": "since", "in": "query", "required": true}}},
            "paths": {"/v1/cassettes/c/reports": {
                "get": {"operationId": "listReports", "parameters": [{"$ref": "#/components/parameters/Since"}]}
            }}
        });
        let cassette = reduce("c", None, &document);
        let param = &cassette.methods[0].params[0];
        assert_eq!(param.wire, "since");
        assert!(param.required);
        assert_eq!(param.location, Location::Query);
    }

    #[test]
    fn a_reference_that_does_not_resolve_is_dropped_rather_than_guessed_at() {
        let document = json!({"paths": {"/v1/cassettes/c/reports": {
            "get": {"operationId": "listReports", "parameters": [{"$ref": "#/components/parameters/Absent"}]}
        }}});
        let cassette = reduce("c", None, &document);
        assert!(cassette.methods[0].params.is_empty());
    }

    #[test]
    fn a_cookie_parameter_is_ignored_because_a_cli_cannot_offer_one() {
        let document = json!({"paths": {"/v1/cassettes/c/reports": {
            "get": {"operationId": "listReports", "parameters": [{"name": "sid", "in": "cookie"}]}
        }}});
        let cassette = reduce("c", None, &document);
        assert!(cassette.methods[0].params.is_empty());
    }

    #[test]
    fn a_parameter_cannot_take_a_flag_the_subcommand_defines_itself() {
        // `--tapes-url` belongs to tapesctl. Handing it to a cassette parameter
        // would make clap panic on a duplicate argument at startup — which the
        // workspace lints forbid and a user could trigger with a custom spec.
        let document = json!({"paths": {"/v1/cassettes/c/reports": {
            "get": {"operationId": "listReports", "parameters": [
                {"name": "tapes_url", "in": "query"},
                {"name": "body", "in": "query"}
            ]}
        }}});
        let cassette = reduce("c", None, &document);
        let flags: Vec<&str> = cassette.methods[0]
            .params
            .iter()
            .map(|p| p.flag.as_str())
            .collect();
        assert_eq!(flags, vec!["param-tapes-url", "param-body"]);
        // The wire names are untouched — only the presentation moved.
        assert_eq!(cassette.methods[0].params[0].wire, "tapes_url");
    }

    #[test]
    fn two_parameters_that_kebab_to_the_same_flag_stay_distinguishable() {
        let document = json!({"paths": {"/v1/cassettes/c/reports": {
            "get": {"operationId": "listReports", "parameters": [
                {"name": "since_id", "in": "query"},
                {"name": "sinceId", "in": "header"}
            ]}
        }}});
        let cassette = reduce("c", None, &document);
        let flags: Vec<&str> = cassette.methods[0]
            .params
            .iter()
            .map(|p| p.flag.as_str())
            .collect();
        assert_eq!(flags, vec!["since-id", "since-id-2"]);
    }

    #[test]
    fn an_operation_without_an_id_gets_one_from_its_verb_and_path() {
        let document = json!({"paths": {"/v1/cassettes/summary/reports/{id}": {"get": {}}}});
        let cassette = reduce("summary", None, &document);
        // The `/v1/cassettes/summary` mount point is dropped: the command
        // already sits under that noun.
        assert_eq!(cassette.methods[0].name, "get-reports-id");
    }

    #[test]
    fn colliding_method_names_are_disambiguated_by_verb() {
        let document = json!({"paths": {"/v1/cassettes/c/thing": {
            "get": {"operationId": "doThing"},
            "post": {"operationId": "do_thing"}
        }}});
        let cassette = reduce("c", None, &document);
        let names: Vec<&str> = cassette.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"do-thing"), "got: {names:?}");
        assert!(
            names.iter().any(|n| n.starts_with("do-thing-")),
            "got: {names:?}",
        );
    }

    #[test]
    fn a_document_with_nothing_usable_yields_a_cassette_with_no_methods() {
        // Not an error: one malformed cassette must not take out the CLI.
        for document in [
            json!({}),
            json!({"paths": {}}),
            json!({"paths": "nonsense"}),
        ] {
            assert!(reduce("c", None, &document).methods.is_empty());
        }
    }

    #[test]
    fn the_generated_surface_is_stable_between_reductions() {
        // A CLI whose subcommand order changed run to run would make `--help`
        // diffs meaningless.
        let first = reduce("hello-world", None, &hello_world());
        let second = reduce("hello-world", None, &hello_world());
        let names =
            |c: &Cassette| -> Vec<String> { c.methods.iter().map(|m| m.name.clone()).collect() };
        assert_eq!(names(&first), names(&second));
    }

    #[test]
    fn kebab_casing_keeps_acronyms_whole() {
        assert_eq!(kebab_case("getHello"), "get-hello");
        assert_eq!(kebab_case("since_id"), "since-id");
        assert_eq!(kebab_case("getHTTPStatus"), "get-http-status");
        assert_eq!(kebab_case("already-kebab"), "already-kebab");
        assert_eq!(kebab_case("X"), "x");
    }
}
