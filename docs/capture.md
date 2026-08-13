---
title: How capture works
description: The two capture lanes, which harness uses which mechanism, what attribution means, and why the session id start prints is not the one read commands take.
sidebar:
  order: 2
---

Capture is two independent lanes recording the same session, plus a sweep that
picks up what neither lane was running for. Understanding which lane a harness
gets — and which of them is optional — explains most of what you will see in
the console, and all of what you will not.

Every command on this page sends to the **ingest** port (`8082` on a local
`tapes serve`), never the read port. See
[The two ports](./introduction.md#the-two-ports).

## The two lanes

### Lane A — the wire

`tapesctl start` launches the harness with its LLM endpoint pointed at a
capture proxy on `127.0.0.1`, on an ephemeral port chosen per launch. Every
eligible call the harness makes is forwarded to the real provider byte for
byte, and a copy is posted to `POST {tapes-url}/v1/ingest`.

This lane *is* `start`. It cannot be turned off, and it is what makes a
session exist at all.

What counts as eligible is a fixed list, and the exclusions matter more than
the inclusions:

- **Paths.** Only these suffixes are turn-eligible: `/v1/chat/completions`,
  `/v1/responses`, `/codex/responses`, `/v1/messages`, `/api/chat`. Health
  probes, model listings, and token counting are forwarded but never recorded.
- **Status.** Only HTTP `200` is capturable — not "any 2xx". A `201` or `204`
  is dropped.
- **Encoding.** Only `gzip`, `x-gzip`, and `zstd` are decoded. A response in
  `br` or `deflate` drops the turn.
- **Size.** Requests are peeked up to 4 MiB and responses read up to 8 MiB. A
  body that exactly fills the request window is treated as incomplete and
  silently uncaptured.

A dropped turn is logged at `debug` and is invisible at the default verbosity.
That is deliberate: a capture failure must never take the harness down, so
nothing on this lane is allowed to raise an error into your session. It also
means "the console is missing a call I know I made" is a question you answer
by re-running with `-v`, not by reading the exit summary.

There is no retry. A turn the ingest server rejects is warned about and gone.

### Lane B — the transcript

Harnesses that keep their own on-disk transcript get a second lane: a tailer
watches that tree while the session runs and posts settled records to
`POST {tapes-url}/v1/ingest/transcript`.

Lane B carries the session's **causal skeleton** — which `Task` tool call
forked which subagent. Lane A cannot: on the wire, a subagent's calls are
indistinguishable from the main thread's, because they are the same shape of
request to the same endpoint.

The cost of losing lane B is specific rather than vague. A measured pair of
identical Claude subagent runs recorded comparable wire lanes — 38 and 40 turns
— and 8 versus **0** transcript turns. Both sessions contain every call the
subagents made. Only one renders that work as nested rows; the other shows flat
dispatch text.

Pass `--no-transcripts` only when another capture client is already tailing the
same tree, since two tailers on one tree is the problem it exists to solve.

### The sweep

`tapesctl sync` is the backstop: it walks completed transcripts on disk and
pushes them, for sessions no capture was running for.

```bash
tapesctl sync --tapes-url http://localhost:8082
```

```
tapesctl: swept 2 session(s), 2 file(s): 2 stored, 0 deduped, 0 failed
```

Unlike `start`, `sync` logs to stderr as usual — only `start` diverts its
diagnostics to a file, and only because a harness owns the terminal.

Two things about `sync` are not visible from its help text:

- **`--since-days` defaults to 7.** The help shows no default and the parsed
  value is genuinely absent, but an absent value maps to seven days. Sweeping a
  transcript tree older than a week gives you a silent partial sweep.
  `--since-days 0` is the "everything" spelling. The window is a cost bound
  only, never a correctness one — widening it is always safe, because the
  server dedups.
- **`sync` can only file Claude sessions.** The harness id it stamps is
  hardcoded. Pointing `--projects-root` at a Codex or pi tree will not do what
  the flag name suggests.

Re-running `sync` is cheap and safe: the ingest endpoint keys rows on a content
hash, so an unchanged transcript comes back `deduped`. `tapesctl` keeps no
client-side ledger of what it has already sent, by design — a ledger that
disagreed with the server would be worse than no ledger.

Any undelivered transcript makes `sync` exit `1`, deliberately, because `sync`
is an explicit request to move data. The summary line still prints first, and
everything that did land is durable.

## Which harness uses which mechanism

There are three capture mechanisms, and a harness is eligible for exactly one.

| harness | mechanism | plugin needed first | lane B | `start` |
|---|---|---|---|---|
| `claude` | endpoint redirect (`ANTHROPIC_BASE_URL`) | none | yes | supported |
| `codex` | argv provider overrides | none | yes | supported |
| `pi` | installed extension | **yes** — `plugin install pi` | no | supported |
| `codex-app` | lifecycle hooks | **yes** — `plugin install codex-app` | no | use `capture` |
| `opencode` | installed plugin | `plugin install opencode` | no | **withdrawn** |

Three consequences worth stating plainly:

- **`claude` and `codex` need no plugin.** Running `plugin install claude`
  tells you so and exits `0` — the ordinary answer, not an error:

  ```
  tapesctl: claude needs no capture plugin — its traffic is captured by redirecting it, which `tapesctl start claude` does.
  ```

- **`pi` and `opencode` get no transcript lane at all.** Neither keeps a
  transcript tree the tailer can read; opencode keeps sessions in SQLite. Their
  subagent structure is not recoverable.

- **`start opencode` is withdrawn** and is refused exactly as an unknown name
  would be. Its registry entry and its plugin still work, and
  `plugin install opencode` still installs — only the `start` verb is gone,
  because on the OAuth path the plugin captures nothing. A capture that runs
  perfectly and records nothing is worse than a refusal.

### Install the plugin before capturing pi

`pi` is captured through an extension file, and `tapesctl` checks for it before
anything is bound or spawned:

```bash
tapesctl start pi --tapes-url http://localhost:8082
```

```
tapesctl: pi cannot be captured until its capture plugin is installed: no plugin at ~/.pi/agent/extensions/tapes-gateway.ts. Run `tapesctl plugin install pi` first.
```

So the order is fixed, once per machine:

```bash
tapesctl plugin install pi
tapesctl start pi --tapes-url http://localhost:8082 -- --provider anthropic --model <model-id>
```

`--provider` and `--model` are pi's own flags, which is why they follow `--`.
Pass both or neither: pi only honours them as a pair, and given one it falls
back to a saved default that may be a provider this capture does not front — so
the session runs and records nothing.

### Capturing an app that launches itself

An app started from the dock has no process for `start` to own, so `codex-app`
uses lifecycle hooks and a long-lived proxy instead. Install once, then run
`capture` for as long as you want the app recorded:

```bash
tapesctl plugin install codex-app
tapesctl capture codex-app --tapes-url http://localhost:8082
```

```
tapesctl: capturing codex-app on 127.0.0.1:64513 — start a session in the app; Ctrl-C to stop
```

`plugin install` packages the hook plugin under `~/.tapes/codex-app/`, writes a
handoff file, points `~/.codex/config.toml` at a loopback port fixed at install
time, and prints the `codex plugin` commands that register it. That endpoint
outlives any one capture, which is why the port cannot be ephemeral the way
`start`'s is — pass `--port` to pin it.

Running `capture codex-app` before installing tells you so:

```
tapesctl: could not read the codex-app handoff at ~/.tapes/codex-app/handoff.json: run `tapesctl plugin install codex-app`
```

If the handoff and the app's configuration disagree about the address,
`capture` refuses rather than warning. A capture bound to one address while the
app talks to another would run perfectly and record nothing.

**`capture` prints no turn counts and no unattributed warning.** Its tally is
constructed but never drained, so unlike `start` it can tell you a session
began but not how much of it landed. Do not describe it as "`start` for apps you
launch yourself" without that caveat.

## Attribution

Attribution is how a captured turn gets filed under the session it belongs to.
The wire lane sees an HTTP request; it does not see a session. Something has to
supply that identity, and what supplies it differs by harness — Claude's is read
from `~/.claude/sessions/<pid>.json`, pi's arrives in an
`X-Tapes-Harness-Session-Id` header.

When that lookup fails, the turn is still captured. It is filed under `unknown`
rather than dropped, and `start` says so on stderr:

```
tapesctl: warning: 3 captured turns could not be attributed to this session and were filed as unknown
```

This warning fires whenever any turn was unattributed — including the mixed
case where a session link is also printed. The link is true and incomplete at
the same time: some of that session's turns are not under it.

If **nothing** was attributed, there is no session to link, and the summary says
what did happen instead:

```
tapesctl: captured 12 turn(s) (3 unattributed — filed as unknown)
```

That is an attribution bug being reported as one, rather than as silence. It is
distinct from `tapesctl: no turns were captured`, which means the wire lane saw
nothing eligible — a harness launched and quit without calling a model.

## Session ids

**The session id `start` prints is not the id read commands take.** This is a
live defect, and until it is fixed the workaround below is the honest
instruction.

`start` prints, and builds its console link from, the **harness's** session id:
Claude's own UUID, or pi's header value. The read API keys sessions on a
different value — the tapes session id — and carries the harness one alongside
it as `harness_session_id`. Feeding the printed id to a read command fails:

```bash
tapesctl sessions get f47ac10b-58cc-4372-a567-0e02b2c3d479 --tapes-url http://localhost:8081
```

```
tapesctl: tapes API returned 404 for http://localhost:8081/v1/sessions/f47ac10b-58cc-4372-a567-0e02b2c3d479: {"error": "session not found", "id": "f47ac10b-58cc-4372-a567-0e02b2c3d479"}
```

To get from a printed id to a usable one, list sessions and match on
`harness_session_id`:

```bash
tapesctl sessions list --tapes-url http://localhost:8081 \
  | jq -r '.items[] | select(.harness_session_id=="f47ac10b-58cc-4372-a567-0e02b2c3d479") | .id'
```

```
01JDQ8F3K2M4N6P8R0T2V4X6Z8
```

The read API does support a `harness_session_id` filter on `/v1/sessions`, but
`tapesctl sessions list` exposes no flag for it today, which is why the match
happens client-side.

A second-order effect is worth knowing when the printed id looks wrong: the id
`start` prints is whichever attributed turn the ingest server accepts **first**.
If a subagent's turn lands ahead of the main thread's, the printed id names the
sub-thread. Listing sessions is reliable where the printed line is not.

## What a capture writes locally

While a harness holds the terminal, nothing may reach stdout or stderr — a
stray log line lands in the middle of a TUI frame. So `start` at default
verbosity writes its diagnostics to a file:

```
~/.tapes/logs/start-YYYYMMDD-HHMMSS-<pid>.log
```

The path is printed before launch and again at exit. Pass `-v` to stream to
stderr instead, accepting what that does to the display; that is the documented
way to watch a capture live.

There is no stderr fallback if the log file cannot be opened. The run prints
`tapesctl: diagnostics disabled — no log file (…)` once and then discards
events, because a corrupted TUI is judged more costly than a lost debugging
session.

`start` does **not** propagate the harness's exit status. A non-zero child is
warned about, and `tapesctl start claude …` still exits `0`. CI wrappers that
treat `start` as transparent to exit status will get this wrong.
