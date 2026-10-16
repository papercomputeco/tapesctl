# Changelog

Notable changes to `tapesctl`. The project is pre-1.0: minor releases may
break, and every breaking change is recorded here plainly.

## Unreleased

### Added

- `tapesctl start pi --model <id>` declares a model the upstream serves that
  pi does not ship. pi runs in `~/.tapes/pi-agent` (persistent; a second live
  launch gets a private temporary directory), where each launch writes a
  `models.json` declaring the model under `openai` with
  `api: "openai-completions"` at the capture proxy, plus the capture
  extension, selects it with `--model openai/<id>`, and removes both at exit.
  Your `settings.json`, `keybindings.json` and user-level instruction files are
  copied in; `auth.json`, `mcp.json` and your `models.json` never are. The
  credential is `$OPENAI_API_KEY` by reference when set, otherwise a
  placeholder. `anthropic/…` and `openai-codex/…` are refused. The capture
  fronts `openai` for a declared model, so its turns are ingested as OpenAI
  even behind an explicit `--upstream`; `--schema anthropic` with `--model` is
  refused. An explicit `--upstream` given as a bare origin gains the `/v1` its
  Chat Completions route needs (one already ending in `/v1` is left alone),
  and one with any other path is refused, since none of the declared model's
  turns could be captured through it. Without `--upstream` the declared
  model's base is the proxy's `openai` route with `/v1`, so its requests reach
  `api.openai.com/v1/chat/completions`. A launch whose config files cannot all
  be written removes the ones it already wrote.
- pi turns carry the working directory and pi's version.

- `tapesctl start hermes` (alias `hermes-agent`). hermes runs in a capture
  home at `~/.tapes/hermes-home` — persistent, so its sessions, memories and
  skills survive between captured launches, and never your own `~/.hermes`,
  from which nothing is copied. Each launch writes a `config.yaml` there whose
  `custom` provider points at the capture proxy, plus the capture plugin, and
  removes both at exit; a second hermes launch while one is live gets a
  private temporary home instead. `--schema openai` (the default, chat completions) or
  `--schema anthropic` picks the wire format; the new `--model` flag names the
  model hermes starts on and applies to hermes only. When `OPENAI_API_KEY` (or
  `ANTHROPIC_API_KEY` for `--schema anthropic`) is set, it is written into that
  config, which is then created `0600`. hermes's auxiliary calls (title
  generation, compression, and the rest) are attributed too, though in a
  process running several sessions at once one may be filed under the most
  recent session rather than its own.
- Launch lineage. Every `start` exports `TAPES_LAUNCH_ID` and
  `TAPES_LAUNCH_HARNESS_ID` to the harness and records the session that launch
  is running in `~/.tapes/launches/<launch-id>.json` while it runs. A `start`
  run from inside a captured session finds that record and stamps the outer
  session on its own turns as `x-tapes-parent-harness-session-id` and
  `x-tapes-parent-harness-id`. The ingest row carries the parent session and,
  when it belongs to another harness, `parent_harness_id`.
- A cross-harness parent is sent only to a tapes ingest whose `GET /ping`
  lists the `parent_harness_id` capability, probed once per launch. Against an
  older tapes the parent is left off the row, as before lineage, instead of
  linking the child to a placeholder session in its own harness; the log and
  the exit summary say so once.
- The vendored ingest contract moves to tapes v0.50.0 (papercomputeco/tapes#393), which adds
  the envelope's optional `parent_harness_id` and the capability list on
  `GET /ping`.

### Changed

- When captured turns cannot reach the ingest server, `start` says so at exit:
  one `tapesctl: error: could not reach the tapes ingest server at <url>; …`
  line on stderr, naming the address it posted to and the `--ingest-url` /
  `TAPES_INGEST_URL` remedy, after the summary line. Turns ingest rejected are
  counted the same way, in a warning that points at the log. Both used to
  reach only the log file, behind a bare `no turns were captured`.
- A captured pi session refuses a model whose requests would not reach the
  capture proxy — a provider the extension does not redirect, or a
  `models.json` provider with its own `baseUrl` — with one line naming the
  provider and host, where it used to warn and send the request uncaptured.
  `TAPES_ALLOW_UNCAPTURED=1` restores the old behaviour.
- A captured hermes session refuses a main-loop or auxiliary model call whose
  base URL is not the capture proxy — a provider switched to inside the
  session, an auxiliary task on its own endpoint — with one line naming the
  host, instead of sending it uncaptured. `TAPES_ALLOW_UNCAPTURED=1` lets such
  calls through.
- `tapes-capture` 0.2 and `tapes-harnesses` 0.2.

- `tapes-client` 0.6.0, the release that follows tapes v0.49.0's paging of
  the composite, trace and raw-turn reads. `sessions traces`, `sessions
  raw-turns` and `traces get` now print **one page** of the server's
  document — the whole document with `next_cursor` on every page but the
  last — and take `--limit` and `--cursor` to page; `traces get`'s record
  view names the command that fetches the next page. `spans list` keeps
  printing the whole trace: it walks every page before it prints.

- Every read command has a human view, and every human view is laid out for
  the terminal it prints on. `sessions list`, `traces list`, and `spans list`
  are borderless tables that drop their least important columns before they
  would wrap; `sessions get`, `traces get`, and `spans get` are record views
  that end by naming the command to run next; `search` is a ranked list with
  the matched snippet under each hit. Colour marks status words only, and is
  applied only on a terminal without `NO_COLOR`. Ids are always printed
  whole. Piped output keeps the alignment, prints the full cursor, and writes
  `-` for absent values. `search` hits carry their session, trace, and span
  ids; `spans get` prints input and output documents unelided. Decoded API
  error text is sanitized before it reaches the terminal.
  `--json` on any of them prints the server's document; `sessions get`,
  `traces *`, `spans *`, and `search` gain the flag, and the JSON they print
  under it is byte-for-byte what they printed by default before. The
  `comfy-table` dependency is gone.
- Costs in a list are cents (`$41.62`, `<$0.01`, or `—` for none); times in a
  list are relative (`2d ago`) and in a record are `Sep 24 05:01` in the offset
  the server sent. A session the deriver has not titled reads `untitled
  (<harness id group>)` instead of the truncated hash the server stores.
- Status lines lost their `tapesctl:` prefix: `✓ captured session <id>` with
  the console link or the `--web-url` note on the next line; `sync` prints
  `Swept <n> sessions (<m> files)` and a `✓ <new> new · <k> unchanged` line;
  `sync -v` per-file lines lead with the outcome, and the outcomes are now
  `new`, `unchanged`, `failed`, `unknown`. Errors and warnings keep the prefix.
- Default logging is `warn`; `-v` is `info` plus tapesctl's own debug, `-vv`
  is trace. The `INFO sweeping transcripts` line `sync` printed on every run is
  gone unless asked for.
- API refusals read `tapes API at <host> answered <status> <reason>: <message>
  (<code>)`, taken from the server's error document, with a `hint:` line for a
  missing cassette, an auth failure, or a 503. The full URL and raw body are no
  longer printed. A refused connection prints the OS reason once and a hint
  naming `tapes serve`, `--api-url`, and `TAPES_API_URL`; the doubled
  `could not reach the tapes API: could not reach the tapes API` and reqwest's
  wrapper lines are gone.

### Added

- `tapesctl sync --harness-id <ID>` (default `claude`) sets the harness id
  stamped on every uploaded transcript. It changes only the label the server
  files sessions under: the sweep still reads the `~/.claude/projects` layout
  and the server still derives Claude-shaped records, so a Codex or pi tree
  pointed at `--projects-root` still finds zero sessions. The flag exists for
  history rewritten into that layout and shape ahead of time.
  `SyncArgs` and `SyncConfig` gain a `harness_id: String` field; code that
  builds either by struct literal must supply it.

### Fixed

- `tapesctl cassettes <name> <method>` no longer fails with `could not decode
  the tapes API response` when the cassette answers with something other than
  JSON. The response is now read by its `Content-Type`: JSON is pretty-printed
  as before, anything else is printed byte-for-byte. `skills
  get-skill-markdown` (`text/markdown`) was unusable before this.

### Removed

- **Breaking:** the top-level cassette spelling `tapesctl <cassette> <method>`
  no longer parses. It shipped as a hidden alias for one release (v0.4.0)
  after `tapesctl cassettes <cassette> <method>` became the canonical form,
  and is now an ordinary unknown-command error. The fix is mechanical: insert
  `cassettes` before the cassette name.

### Changed

- Cassette discovery — the surface cache read and any revalidation request —
  now runs only for the command lines that can reach the generated surface:
  `tapesctl cassettes …`, `tapesctl help …`, and bare or flags-only
  invocations. Every other command builds its CLI with zero discovery I/O.
  Retiring the top-level aliases is what made this possible: while any first
  token could have been a cassette, every invocation had to discover before
  it could parse.
