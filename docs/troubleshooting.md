---
title: Troubleshooting
description: The failures that actually happen — a capture that landed nowhere, a session id that 404s, a missing server URL, and plugin conflicts.
sidebar:
  order: 6
---

Ordered by how often they bite and how long they take to diagnose.

## A capture ran, but the session is not there

**Symptom.** `tapesctl start` finished without complaint and exited `0`, and
`tapesctl sessions list` does not show the session.

**First check the exit line.** `start` says which of two things happened:

```
tapesctl: no turns were captured
```

That means nothing landed in the server. It is *not* the same as
`tapesctl: captured session …`, which means it did.

**The usual cause is the wrong port.** Ingest and reads are separate listeners
— `8082` and `8081` on a local `tapes serve`. `start`, `capture`, and `sync`
all want **ingest**. Pointed at the read port, every turn is posted to a route
that does not exist there, every post is rejected, nothing is counted, and the
command still exits `0`, because a telemetry failure is never allowed to take
your harness down.

Confirm it in the log file, whose path `start` prints twice:

```bash
grep "ingest rejected the turn" ~/.tapes/logs/start-*.log
```

That message is logged at warning level, so it is present at default
verbosity. Then re-run against ingest:

```bash
tapesctl start claude --tapes-url http://localhost:8082
```

**Other causes, in the order worth checking:**

- **The call was never eligible.** Only these path suffixes are captured:
  `/v1/chat/completions`, `/v1/responses`, `/codex/responses`, `/v1/messages`,
  `/api/chat`. Only HTTP `200` — not any 2xx. Only `gzip`, `x-gzip`, and `zstd`
  content encodings; `br` and `deflate` drop the turn. These drops are logged at
  `debug`, so they are invisible until you re-run with `-v`.
- **The harness never called a model.** Launched and quit produces exactly the
  same line, honestly.
- **You are reading a different server than you wrote to.** With `TAPES_URL`
  exported, a `--tapes-url` on the read command overrides it, and vice versa —
  check both. `tapesctl config get tapes-url` shows the configured fallback.

## `sessions get` 404s the id that `start` printed

**Symptom.**

```
tapesctl: tapes API returned 404 for http://localhost:8081/v1/sessions/f47ac10b-58cc-4372-a567-0e02b2c3d479: {"error": "session not found", ...}
```

**Cause.** The id `start` prints is the **harness's** session id. Read commands
take the **tapes** session id. They are different values in different
namespaces, and the console link `start` builds uses the harness one too. This
is a live defect, not a mistake on your part.

**Workaround.** Pass the printed id to `sessions list` as the
`--harness-session-id` filter — paired with `--harness-id`, the harness you
launched, because the server accepts the harness filter only whole — and read
the `id` on the result:

```bash
tapesctl sessions list --harness-id claude \
  --harness-session-id f47ac10b-58cc-4372-a567-0e02b2c3d479 \
  --tapes-url http://localhost:8081 | jq -r '.items[].id'
```

```
01JDQ8F3K2M4N6P8R0T2V4X6Z8
```

Use that `id` with every read command. The filter is the read API's own
`harness_session_id` parameter, applied server-side; a printed id that matches
nothing returns an empty `items`. A lone half of the pair fails at parse with
the missing half named — that is `tapesctl` refusing a shape the server would
400.

**If the printed id looks like the wrong session entirely**, it may be: the id
`start` prints is whichever attributed turn ingest accepts *first*, so a
subagent's turn landing ahead of the main thread's names the sub-thread.
Listing is reliable where the printed line is not. See
[Session ids](./capture.md#session-ids).

## `no tapes server URL`

**Symptom.**

```
tapesctl: no tapes server URL: pass --tapes-url, set TAPES_URL, or configure a default with `tapesctl config set tapes-url <url>`
```

**Cause.** None of the three sources named a server. `tapesctl` never guesses a
host — a capture pointed at whatever happened to be listening is worse than one
that did not start.

**Fix.** Any of the three. The third is the one worth doing:

```bash
tapesctl config set tapes-url http://localhost:8081
```

**If you set it and still get this**, check which one you set and which one the
command wants. Remember a configured default holds one URL while the deployment
has two ports; capture commands may still need an explicit
`--tapes-url http://localhost:8082`.

**If `config get` prints nothing but the file is not empty**, that is expected:
only keys that are both known and set are listed. A file containing only keys
this build does not know produces empty output. `tapesctl config path` shows
where to look.

## `could not reach the tapes API: could not reach the tapes API`

The doubled clause is real, not a copy-paste error in this page.

**Cause.** The URL resolved, but nothing answered — wrong port, server down, or
a host that does not route. Check the server is up and that you named the
listener you meant.

Note that a **path prefix in the URL is discarded**:
`--tapes-url http://host/base/` reads from `http://host/v1/sessions`, not
`/base/v1/sessions`. If your deployment is mounted under a path, that is a
server-side routing question, not something the flag can express.

## `pi cannot be captured until its capture plugin is installed`

**Symptom.**

```
tapesctl: pi cannot be captured until its capture plugin is installed: no plugin at ~/.pi/agent/extensions/tapes-gateway.ts. Run `tapesctl plugin install pi` first.
```

**Cause and fix.** Exactly what it says, and the check runs before anything
binds or spawns, so nothing was half-started:

```bash
tapesctl plugin install pi
```

**A pi session that runs but records nothing** is a different problem: pass
`--provider` and `--model` together or not at all. They are pi's own flags, so
they follow `--`. Given only one, pi ignores it and falls back to a saved
default that may be a provider this capture does not front.

```bash
tapesctl start pi --tapes-url http://localhost:8082 -- --provider anthropic --model <model-id>
```

## Plugin and extension conflicts

**Two capture clients tailing one transcript tree.** Only one should. Pass
`--no-transcripts` to whichever should stand down — that is the case the flag
exists for.

**A stale plugin copy under another name.** pi loads every file in its
extension directory into one process, so a superseded copy is a second reader
contending over the same launch nonce. `plugin install` removes superseded
copies before renaming the new one into place and says so:

```
tapesctl: removed superseded <path>
```

If you copied a gateway file by hand, remove your copy.

**`plugin install` refuses a flag.** `--port` and `--codex-auth` apply only to
hook-plugin harnesses. On a file-copy harness they are refused rather than
ignored, because a flag that silently does nothing reads exactly like a flag
that worked:

```
tapesctl: --port does not apply to pi, whose capture plugin is a file copy
```

The same rule governs `--schema` on `claude` and `codex`.

**`plugin install claude` "does nothing".** It reports that no plugin is needed
and exits `0`. That is the ordinary answer, not a failure — `claude` and
`codex` are captured by redirection.

**`codex-app` is still registered after uninstall.** `plugin uninstall
codex-app` removes the provider entry and the state directory, but leaves the
plugin registered with Codex; its `--help` line about "any configuration it
wrote" overstates what happens. The command prints the remaining step:

```
codex plugin remove tapesctl-codex-app@tapesctl
```

**`capture codex-app` refuses to start.** Either the handoff is missing —

```
tapesctl: could not read the codex-app handoff at ~/.tapes/codex-app/handoff.json: run `tapesctl plugin install codex-app`
```

— or the handoff and the app's own configuration disagree about the address, in
which case `capture` refuses rather than warns. A capture bound to one address
while the app talks to another would run perfectly and record nothing.

## Subagent work renders as flat text

**Cause.** The transcript lane is missing. It is the only source of a session's
causal skeleton — which `Task` call forked which subagent — and the wire lane
cannot recover it, because on the wire a subagent's calls are the same shape of
request as the main thread's.

Either the harness has no transcript lane at all (`pi` and `opencode` do not),
or `--no-transcripts` was passed, or nothing was tailing when the session ran.

**For a Claude session that already ended**, sweep it up:

```bash
tapesctl sync --tapes-url http://localhost:8082 --since-days 0
```

`sync` is safe to repeat — the server dedups on a content hash.

## `sync` says it swept less than expected

**`--since-days` defaults to 7**, and `--help` does not say so. A tree older
than a week is silently partially swept. Use `--since-days 0` for everything;
the window is a cost bound, never a correctness one.

**`sync` files Claude sessions only.** The harness id it stamps is hardcoded,
so `--projects-root` pointed at a Codex or pi tree will not do what the flag
name suggests.

**`sync` exited 1 but the summary looked fine.** Any undelivered transcript
fails the command, deliberately, because `sync` is an explicit request to move
data. The summary prints first and everything that landed is durable — re-run
to retry the rest.

## `search` returns 503

The deployment has no span embeddings. The response body says which of the two
causes it is: no embedder or store configured, or no embedding pass has run.
It surfaces as:

```
tapes API returned 503 for …: <body>
```

An empty result set is not an error — that prints `No results found.` and exits
`0`.

## `capture` will not tell me how much it captured

It cannot. Unlike `start`, `capture` prints no turn counts and no unattributed
warning, because its tally is never drained. It reports that a session began
and, at the end, how many sessions it saw. To confirm what landed, read the
server.

## `start` exited 0 but my harness failed

`start` does not propagate the harness's exit status. A non-zero child is
warned about and `start` still exits `0`. CI wrappers that treat `start` as
transparent to exit status need to check the harness themselves.

## Which build am I running?

You cannot tell from the binary. `tapesctl version` and `tapesctl --version`
both report `0.1.0` for every release to date, because the crate version has
never been bumped while releases are tagged independently. Treat `0.1.0` in a
bug report as version-less, and record where you got the binary instead.

## Turning on more detail

```bash
tapesctl -v sessions list --tapes-url http://localhost:8081   # debug
tapesctl -vv sessions list --tapes-url http://localhost:8081  # trace
RUST_LOG=debug tapesctl sessions list --tapes-url http://localhost:8081
```

`RUST_LOG` overrides the `-v` count. A set-but-empty `RUST_LOG` is treated as
unset; an unparseable one warns and falls back.

For `start`, `-v` also changes *where* logs go: at default verbosity it writes
to `~/.tapes/logs/start-*.log` because the harness owns the terminal, and `-v`
streams to stderr instead. If you see

```
tapesctl: diagnostics disabled — no log file (<err>)
```

the file could not be opened and events are being discarded — there is no
stderr fallback, because a corrupted display is judged worse than a lost
debugging session. Re-run with `-v` to get the events on stderr.
