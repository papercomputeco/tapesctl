---
title: Command reference
description: Every tapesctl command with its flags, environment equivalents, exit codes, and error families.
sidebar:
  order: 3
---

Fourteen top-level commands, plus whatever cassette surface your deployment
serves. This page is the whole reference; [Capture](./capture.md) explains the
concepts behind `start`, `capture`, and `sync`, and
[Cassettes](./cassettes.md) covers the discovered surface.

Read commands use `--api-url`; capture commands use `--ingest-url`. See
[The two ports](./introduction.md#the-two-ports).

## Global flags

Both are declared globally, so they may be given before or after the
subcommand and reach every leaf.

| flag | type | default | notes |
|---|---|---|---|
| `-v`, `--verbose` | count | `0` | `-v` is `debug`, `-vv` is `trace`. `RUST_LOG` overrides both |
| `--api-url <URL>` | string | `http://localhost:8081` | falls back to `TAPES_API_URL`, then `config.toml` |
| `-h`, `--help` | flag | — | |
| `-V`, `--version` | flag | — | prints one line; see [`version`](#version) before trusting it |

Both are also read straight off the argument list before parsing, and both stop
at a bare `--`. A harness's own `-v` or `--api-url` after the separator
cannot steer `tapesctl`.

**Leaf position beats global position.** Given both,
`tapesctl --api-url A sessions list --api-url B` uses `B`.

**`--api-url` appears in the help of commands that never make an HTTP
call** — `config set`, `config get`, `config path`,
`version`, and `plugin uninstall` — because the global flag propagates into
every leaf's help. It is inert there. Its presence in `config`'s help is
actively misleading, since the point of `config set api-url` is that you do
not have a server configured yet.

## Exit codes

Three values, and only three.

| code | meaning |
|---|---|
| `0` | success |
| `1` | a runtime error — one line on stderr, prefixed `tapesctl: ` |
| `2` | an argument-parsing error, or help printed because a subcommand was missing |

**A bare `tapesctl` prints help and exits `2`.** So does `tapesctl sessions`,
`tapesctl cassettes`, or any other noun given without a verb. Scripts under
`set -e` should call `tapesctl version` to check for the binary, not a bare
invocation.

`start` does not propagate its harness's exit status: a non-zero child is
warned about and `start` still exits `0`.

## Environment variables

Every variable `tapesctl` reads.

| variable | read by |
|---|---|
| `TAPES_API_URL` | every read command, cassette discovery, and generated methods |
| `TAPES_INGEST_URL` | `start`, `capture`, `sync` |
| `TAPES_UPSTREAM` | `start`, `capture` |
| `TAPES_WEB_URL` | `start`, `capture` |
| `TAPES_ORG_ID` | `start`, `capture` |
| `TAPES_AUTH_SUBJECT` | `start`, `capture`, `sync` |
| `RUST_LOG` | logging, all commands |
| `TAPESCTL_CACHE_DIR` | the cassette surface cache |
| `CODEX_HOME` | `plugin install`/`uninstall codex-app`, `capture codex-app`, `start codex` |
| `USER`, then `USERNAME` | the default `--auth-subject` (`local:<user>`, else `local:unknown`) |
| `OPENAI_API_KEY` | `start codex` upstream selection |

There is no telemetry variable, because there is no telemetry.

---

## start

Launch a harness under a capture proxy and ship its turns to the ingest server.

```bash
tapesctl start claude --ingest-url http://localhost:8082
tapesctl start claude --ingest-url http://localhost:8082 -- --model opus
```

Anything after `--` is passed to the harness verbatim.

| flag | default | env |
|---|---|---|
| `<HARNESS>` | required — `claude`, `codex`, or `pi` | — |
| `[HARNESS_ARGS]...` | — | — |
| `--ingest-url <URL>` | `http://localhost:8082` | `TAPES_INGEST_URL` |
| `--upstream <URL>` | the harness's own provider API | `TAPES_UPSTREAM` |
| `--schema <SCHEMA>` | the harness's own | — |
| `--web-url <URL>` | none | `TAPES_WEB_URL` |
| `--org-id <UUID>` | `""` — the server's local sentinel org | `TAPES_ORG_ID` |
| `--auth-subject <S>` | `local:<username>` | `TAPES_AUTH_SUBJECT` |
| `--no-transcripts` | off | — |

`--schema` is `anthropic` or `openai`, and only applies to a harness that
redirects several providers to one endpoint — `pi`. On `claude` or `codex` it
is an error, not a no-op.

`--web-url` is used only to build the printed console link.

Endpoints: `POST {ingest-url}/v1/ingest` for the wire lane,
`POST {ingest-url}/v1/ingest/transcript` for the transcript lane. Neither sends
an authentication header. The proxy listens on `127.0.0.1:0` — an ephemeral
port, per launch.

**A base path in `--ingest-url` is discarded.** `--ingest-url http://host:8090/base/`
posts to `http://host:8090/v1/ingest`, not `/base/v1/ingest`. Upstream
forwarding is the opposite and concatenates, so an upstream route prefix
survives.

### What start prints

Before launch, and only when diagnostics went to a file:

```
tapesctl: capturing; logs at ~/.tapes/logs/start-20260813-180411-54233.log
```

Between spawn and harness exit, nothing at all. At exit, exactly one of:

```
tapesctl: no turns were captured
tapesctl: captured session <id> — <console url>
tapesctl: captured session <id> (pass --web-url for a console link)
tapesctl: captured <n> turn(s) (<u> unattributed — filed as unknown)
```

then, on stderr when any turn was unattributed:

```
tapesctl: warning: <u> captured turn(s) could not be attributed to this session and were filed as unknown
```

then, on stderr only if the shutdown drain gave up:

```
tapesctl: warning: <n> turn(s) still being captured at exit; the counts above may be short
```

and finally, on stdout, `tapesctl: logs at <path>`.

The printed session id is the **harness's**, not the one read commands take —
see [Session ids](./capture.md#session-ids).

### start errors

All exit `1`.

| message | when |
|---|---|
| `unsupported harness "X" (supported: claude, codex, pi)` | unknown name, or `opencode` |
| `--schema does not apply to claude, which speaks anthropic only (it is for a harness that redirects several providers to one endpoint, such as pi)` | `--schema` on `claude` or `codex` |
| `invalid --schema "X" (valid values: anthropic, openai)` | bad `--schema` value |
| `pi cannot be captured until its capture plugin is installed: no plugin at <path>. Run `tapesctl plugin install pi` first.` | the pi extension is absent — checked before anything binds or spawns |
| `could not bind the capture proxy` / `could not start <harness>` | loopback bind or spawn failure |

**Capture failures never appear here.** An oversize body, an ingest rejection, a
non-JSON request body — each is logged and the turn is skipped, because a
telemetry failure must never take the harness down.

## capture

Bind the address a self-launching harness was installed against, and capture
whichever sessions run in that window. Today the only harness is `codex-app`.

```bash
tapesctl capture codex-app --ingest-url http://localhost:8082
```

A deliberate subset of `start`'s flags: there is no `--schema`, no
`--no-transcripts`, and no trailing-argument passthrough — `tapesctl capture
codex-app -- -p hi` is a parse error.

| flag | default | env |
|---|---|---|
| `<HARNESS>` | required | — |
| `--ingest-url <URL>` | `http://localhost:8082` | `TAPES_INGEST_URL` |
| `--upstream <URL>` | the backend honouring the configured credential | `TAPES_UPSTREAM` |
| `--web-url <URL>` | none | `TAPES_WEB_URL` |
| `--org-id <UUID>` | `""` | `TAPES_ORG_ID` |
| `--auth-subject <S>` | `local:<username>` | `TAPES_AUTH_SUBJECT` |

Prints `tapesctl: capturing <harness> on <addr> — start a session in the app;
Ctrl-C to stop`, then one line per session, then
`tapesctl: stopped after <n> session(s)`.

**There is no exit summary** — no turn counts and no unattributed warning,
because `capture`'s tally is never drained.

Errors (exit `1`) include `unknown harness "X" (known: claude, codex,
codex-app, opencode, pi)`, a not-a-hook-harness refusal, and five handoff
failures that each end by naming `tapesctl plugin install codex-app`. A
mismatch between the handoff address and the app's own configuration is refused
rather than warned about.

## sync

Sweep completed Claude transcripts on disk into the ingest server.

```bash
tapesctl sync --ingest-url http://localhost:8082
tapesctl sync --ingest-url http://localhost:8082 --since-days 0
```

| flag | default | env |
|---|---|---|
| `--ingest-url <URL>` | `http://localhost:8082` | `TAPES_INGEST_URL` |
| `--projects-root <PATH>` | `~/.claude/projects` | — |
| `--auth-subject <S>` | `local:<username>` | `TAPES_AUTH_SUBJECT` |
| `--since-days <N>` | **7** — see below | — |

**`--since-days` defaults to 7, and `--help` does not say so.** The declaration
carries no default and the parsed value is genuinely absent; an absent value is
mapped to seven days downstream. `--since-days 0` sweeps everything. The window
is a cost bound, never a correctness one.

**`sync` files Claude sessions only** — the harness id it stamps is hardcoded,
so `--projects-root` pointed at another harness's tree will not do what the
name suggests.

Prints one line:

```
tapesctl: swept 2 session(s), 2 file(s): 2 stored, 0 deduped, 0 failed
```

Any failure then exits `1` with `<n> of <m> transcript(s) could not be
delivered`. The summary prints first, and everything that landed is durable.
Deduplication is entirely server-side, keyed on a content hash; a dedup counts
as a success.

## sessions

Read commands. `sessions list` renders its listing as a table by default, with
`--json` restoring the raw document; every other command prints the server's
JSON pretty-printed. Responses are never re-modelled on the way through, so
fields the server grows reach you without a client upgrade.

| leaf | route | flags |
|---|---|---|
| `list` | `GET /v1/sessions` | `--limit`, `--cursor`, `--sort`, `--direction`, `--since`, `--until`, `--harness-id`, `--harness-session-id`, `--auth-subject`, `--json` |
| `get <ID>` | `GET /v1/sessions/{id}` | — |
| `traces <ID>` | `GET /v1/sessions/{id}/traces` | `--payload` |
| `raw-turns <ID>` | `GET /v1/sessions/{id}/raw_turns` | — |

```bash
tapesctl sessions list --limit 20 --api-url http://localhost:8081
tapesctl sessions get 01JDQ8F3K2M4N6P8R0T2V4X6Z8 --api-url http://localhost:8081
```

`sessions list` flags are all optional and all omitted from the query when
unset, so the server's own defaults apply. The one coupling: the server accepts
the harness filter only whole, so `--harness-id` and `--harness-session-id`
come as a pair — a lone half fails at parse with the missing half named.

| flag | behaviour |
|---|---|
| `--limit <N>` | the server defaults to 50 and clamps at 200 |
| `--cursor <C>` | only valid with the `--sort` and `--direction` it was minted under; changing either is a 400 |
| `--sort <COL>` | e.g. `last_active`, `started_at`, `total_cost_usd` |
| `--direction <D>` | `asc` or `desc` |
| `--since`, `--until` | RFC 3339 |
| `--harness-id <H>` | the harness the session ran under (e.g. `claude`) — the other half of the pair |
| `--harness-session-id <ID>` | exact match on the harness session id — the id `start` prints; pairs with `--harness-id`; see [Session ids](./capture.md#session-ids) |
| `--auth-subject <S>` | exact match |
| `--json` | print the raw pretty-printed JSON instead of the table, so the output still composes with `jq` |

`--payload` takes `full` (the default) or `preview`, case-insensitively. An
unknown value fails **before any request is made**:

```
tapesctl: invalid --payload "bogus" (valid values: full, preview)
```

`sessions traces` is what the console renders; `sessions raw-turns` is the wire
turns behind that derivation.

The read API carries **no authentication**, and redirects are refused rather
than followed. A base path in `--api-url` is discarded here too.

## traces

| leaf | route | flags |
|---|---|---|
| `list <SESSION_ID>` | `GET /v1/traces?session_id=` | — |
| `get <TRACE_ID>` | `GET /v1/traces/{trace_id}` | `--payload` |

## spans

| leaf | route | flags |
|---|---|---|
| `list <TRACE_ID>` | `GET /v1/traces/{trace_id}`, projected to its `spans` array | `--payload` |
| `get <TRACE_ID> <SPAN_ID>` | `GET /v1/traces/{trace_id}/spans/{span_id}` | — |

**`spans list` is a projection, not a route.** The API has no standalone span
collection — spans exist only inside a trace — so the command fetches the trace
and prints its `spans`. A trace with no `spans` key prints `[]` rather than
failing.

`spans get` takes **two** positionals. The trace id is not optional.

## search

Semantic search over captured spans. Hits are individual main-conversation LLM
spans with their trace and turn context.

```bash
tapesctl search "how to configure logging" --api-url http://localhost:8081
tapesctl search "error handling patterns" --top 10 --api-url http://localhost:8081
```

| flag | default | notes |
|---|---|---|
| `<QUERY>` | required | |
| `-k`, `--top <N>` | `5` | the server has no ceiling on this |
| `-q`, `--quiet` | off | one bare session id per line, deduplicated in score order |

Route: `GET /v1/cassettes/search/spans?query=&top_k=` — the search cassette's
serving of the span-search contract. Both parameters are always sent.

`--quiet` is a **pipe format, not a verbosity setting**: one bare session id
per line, deduplicated in score order, ready for command substitution into
anything that takes session ids — for example the skills cassette's generate
operation (`tapesctl skills --help` shows its current shape).

Non-quiet output is a ranked list — rank, score to four decimals, `trace/span`
ids, the turn's prompt elided at 80 characters, a snippet elided at 100, then
the start time and session id. A turn with an empty prompt renders as
`(synthetic turn)`; the server sends the field even when blank precisely so the
case stays distinguishable. Treat printed scores as display values, not as
exact numbers to assert on.

**An empty result set is not an error**: non-quiet prints `No results found.`
and exits `0`; quiet prints nothing and exits `0`.

A deployment without span embeddings answers `503`, and the body says which of
the two causes it is. It surfaces as `tapes API returned 503 for …: <body>`.

`-k -1` is refused by the parser, with clap's `unexpected argument '-1' found`
and a `-- -1` tip rather than a range complaint.

## export

Write a session's export bundle — JSONL, one line per trace — to a file or
stdout.

```bash
tapesctl export 01JDQ8F3K2M4N6P8R0T2V4X6Z8 -o bundle.jsonl --api-url http://localhost:8081
```

| flag | default |
|---|---|
| `<SESSION_ID>` | required |
| `--detail <GRAIN>` | the server's default, `spans` |
| `-o`, `--output <PATH>` | stdout |

`--detail` takes `spans` or `traces`, case-insensitively. Anything else fails
before the request:

```
tapesctl: invalid --detail "everything" (valid values: spans, traces)
```

The body is streamed rather than buffered, and a non-success status is read and
surfaced **before any bytes are written**, so an error page can never land in
your output file. The bundle is written verbatim — the console and the importer
both parse it, so even reserializing the JSON would break them.

**With `-o`, the byte count goes to stderr**, keeping stdout redirection clean —
the line is `tapesctl: wrote <n> bytes to <path>`. So
`tapesctl export <id> -o f.jsonl > log` captures nothing in `log`.

## seed

Populate a server with demo sessions so a fresh console has something to
render. `POST /v1/admin/seed/demo` — an **admin route on the read API**, not on
ingest.

```bash
tapesctl seed --api-url http://localhost:8081
```

```
tapesctl: seeded 4 session(s) (128 raw turns: 128 inserted, 0 deduped) into http://localhost:8081/
```

Every count is read defensively, so a server that trims a field cannot turn a
successful seed into a failure. Re-seeding reports everything `deduped`.

This writes into the server's single-tenant org. It is not something to point
at a populated deployment. A server without the raw-turn layer answers `501`,
surfaced with its body.

## skills

Skills are served by the skills cassette, so the `skills` command is not
hand-built here: it is generated from the cassette's own OpenAPI document at
invocation time, exactly like every other [cassette command](#cassettes). Run
`tapesctl skills --help` against a deployment serving the cassette to see the
verbs it currently vends — a new cassette capability reaches this CLI with no
release. The retired local `skill` verb family (`generate`/`list`/`sync`,
which authored files under `~/.tapes/skills/`) has been removed.

## plugin

### plugin install

```bash
tapesctl plugin install pi
tapesctl plugin install codex-app --dry-run
```

| flag | default | applies to |
|---|---|---|
| `--dry-run` | off | all |
| `--port <N>` | a free port chosen and recorded at install time | hook-plugin harnesses only (`codex-app`) |
| `--codex-auth <M>` | `chatgpt` | hook-plugin harnesses only |

`--port` and `--codex-auth` are **refused, not ignored**, for a file-copy
harness:

```
tapesctl: --port does not apply to pi, whose capture plugin is a file copy
```

`--codex-auth` takes `chatgpt` or `api-key`; anything else gives `invalid
--codex-auth "X" (valid values: chatgpt, api-key)`.

Harnesses captured by redirection report that they need nothing, and exit `0`:

```
tapesctl: claude needs no capture plugin — its traffic is captured by redirecting it, which `tapesctl start claude` does.
```

Do not present `plugin install` as a required step for `claude` or `codex`.

The install is atomic: contents go to a staging file created exclusively,
permissions are set through the handle, superseded copies are removed, then the
file is renamed over the target — so no failure leaves a harness with a missing
or half-written plugin. Superseded copies are removed *before* the rename,
because pi loads every file in its extension directory into one process and a
stale copy under another name is a second reader contending for the same launch
nonce. Each removal prints `tapesctl: removed superseded <path>`.

The harness name is resolved before the machine is, so a typo neither reads
your home directory nor looks for `codex` on `PATH`.

`plugin install opencode` still works, even though `start opencode` is
withdrawn.

### plugin uninstall

One flag, `--dry-run`.

**Uninstall is not complete removal for `codex-app`.** The Codex plugin
registration survives and must be removed by hand; the command prints the
exact incantation:

```
tapesctl: would remove the "tapesctl-codex-app" provider from ~/.codex/config.toml
tapesctl: would remove ~/.tapes/codex-app
tapesctl: would leave the plugin registered with Codex; remove it with `codex plugin remove tapesctl-codex-app@tapesctl`
```

Its `--help` says "and any configuration it wrote", which overstates this.

### plugin hook

Hidden, and machine-only. It reports one lifecycle event to a running capture
proxy, is invoked by an installed hook plugin, and reads its event payload from
stdin — so a person typing it has nothing to pipe in. `--handoff <PATH>` is
required.

It is listed here so that finding it in a process list or in a Codex config
identifies it, not so that you run it.

## config

Key and value, following `git config` and `gh config` rather than a flag per
setting. Needs no server — requiring `--api-url` to configure `--api-url`
would be a circle.

| leaf | args | behaviour |
|---|---|---|
| `set <KEY> <VALUE>` | both required | validates the key, then the URL scheme, then edits the file in place; prints `<key> = <value>` |
| `get [KEY]` | key optional | with a key, prints the value or nothing; without, prints every known **and set** key |
| `path` | none | prints the path whether or not the file exists |

```bash
tapesctl config set api-url http://localhost:8081
tapesctl config get api-url
tapesctl config path
```

```
/Users/you/.tapes/config.toml
```

Validation, all exiting `1` and writing nothing:

```
tapesctl: unknown config key "tapes-erl" (known keys: api-url)
tapesctl: invalid tapes URL
tapesctl: api-url must be an http or https URL; "ftp" is not a scheme this client can call
```

**A known-but-unset key prints nothing and exits `0`**, so
`$(tapesctl config get api-url)` is empty rather than an error a script has
to special-case. That also means `config get` can print nothing from a file
that is not empty — only known and set keys are listed. See
[Configuration](./configuration.md).

## version

```bash
tapesctl version
```

```
tapesctl 0.1.0
All in all, just another tape in the stereo
```

Both lines are expected; the second is the release smoke test's canary and is
pinned as an exact string. `--version` prints only the first line.

**The number is not a release identifier.** It comes from the crate version,
which has never been bumped, while releases are tagged independently — so a
binary from any release reports `0.1.0`. Do not tell anyone to "check your
version with `tapesctl --version`", do not pin documentation to a version the
binary can confirm, and treat `0.1.0` in a bug report as version-less. To
identify a build, record where you got it.

## cassettes

The command surface your deployment serves, discovered from the server at
runtime. Covered in full in [Cassettes](./cassettes.md).

```bash
tapesctl cassettes --help                 # what this server serves
tapesctl cassettes <name> --help          # that cassette's methods
tapesctl cassettes <name> <method>        # call one
```

The noun is always mounted, even with nothing under it, so `tapesctl cassettes`
is never an unknown-command error. Bare — with no subcommand — it exits `2`,
naming the discovered set it wanted.

## Error families

Every runtime error is one line on stderr prefixed `tapesctl: `, and exits `1`.

| family | shape |
|---|---|
| unreachable server | `could not reach the tapes API: could not reach the tapes API` |
| non-success status | `tapes API returned <status> for <endpoint>: <body>` |
| invalid flag value | `invalid --<flag> "<value>" (valid values: …)` — raised before any request |
| inapplicable flag | `--<flag> does not apply to <harness>, …` — refused, never silently ignored |
| unknown harness | `unsupported harness "X" (supported: …)` from `start`; `unknown harness "X" (known: …)` from `capture` |
| missing plugin | `<harness> cannot be captured until its capture plugin is installed: …` |

The doubled clause in the unreachable-server message is real, not a
transcription error here.

The no-server message names all three sources, and is the main place a user
learns `config set` exists.
