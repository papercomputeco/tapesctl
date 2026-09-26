# Changelog

Notable changes to `tapesctl`. The project is pre-1.0: minor releases may
break, and every breaking change is recorded here plainly.

## Unreleased

### Changed

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
