---
title: tapesctl
description: The command-line client for tapes — what it does, the two server ports it talks to, and a first capture and read.
sidebar:
  order: 1
---

`tapesctl` is the command-line client for [tapes](https://tapes.dev). Tapes
records what coding agents actually did — every LLM call an agent made, as
sessions, traces, and spans. The server stores and serves that data;
`tapesctl` is what captures it and what reads it back.

The split is worth stating once, because it decides which documentation
answers a given question:

- **tapes** is the server. It owns `serve`, `local up`, `seed`, and the HTTP
  APIs. Running one is documented in the server's own docs at
  <https://tapes.dev/docs/tapes/>.
- **tapesctl** is the client. Every capture and read verb lives here.

You bring your own server. `tapesctl` never guesses one: with no server
configured, commands that need one refuse to run rather than send a capture to
whatever happens to be listening.

## The two ports

A tapes deployment serves reads and ingest on **separate listeners**, and
`tapesctl` commands are split across them. Passing the wrong one is the single
most expensive mistake available here: a capture pointed at the read port still
exits `0`. It reports `no turns were captured`, and the reason — every turn
rejected by a route that does not exist there — is a warning in a log file you
were not watching.

| port | listener | the commands that use it |
|---|---|---|
| `8081` | read API | `sessions`, `traces`, `spans`, `search`, `export`, `seed`, `skill generate`, `cassettes` |
| `8082` | ingest | `start`, `capture`, `sync` |

Those are the defaults of a local `tapes serve`. A deployment that fronts both
behind one hostname gives you one URL for everything; check with whoever runs
it. What does not vary is which side of the split a command is on.

Because a configured default holds one URL, a machine that both captures and
reads a local server cannot name both with configuration alone. Configure the
one you type least often and pass the other explicitly. See
[Configuration](./configuration.md).

## Install

```bash
curl -sSfL https://download.tapes.dev/tapesctl/install | bash
```

Confirm it landed:

```bash
tapesctl version
```

```
tapesctl 0.1.0
All in all, just another tape in the stereo
```

Both lines are expected. The second is the release smoke test's canary.

The version number is **not** a release identifier — every release to date
reports `0.1.0`, because the crate version has never been bumped and releases
are tagged independently. Do not use `tapesctl --version` to work out which
build you have, and do not treat `0.1.0` in a bug report as meaningful. Read
[the version trap](./commands.md#version) before relying on it for anything.

Supported platforms are Linux and macOS, on x86-64 and arm64.

## Two minutes: capture, then read

Capture a Claude session. The harness behaves as it would unproxied — its
traffic is forwarded to its own provider API — and the capture proxy dies with
it. The URL is the **ingest** port:

```bash
tapesctl start claude --tapes-url http://localhost:8082
```

Before the harness launches, and again when it exits, `tapesctl` prints to
stdout; while the harness holds the terminal it prints nothing at all:

```
tapesctl: capturing; logs at ~/.tapes/logs/start-20260813-180411-54233.log
tapesctl: captured session f47ac10b-58cc-4372-a567-0e02b2c3d479 (pass --web-url for a console link)
tapesctl: logs at ~/.tapes/logs/start-20260813-180411-54233.log
```

Now read it back, against the **read** port:

```bash
tapesctl sessions list --limit 20 --tapes-url http://localhost:8081
```

```json
{
  "items": [
    {
      "auth_subject": "local:jasonwc",
      "harness_id": "claude",
      "harness_session_id": "f47ac10b-58cc-4372-a567-0e02b2c3d479",
      "id": "01JDQ8F3K2M4N6P8R0T2V4X6Z8",
      "last_seen_at": "2026-08-13T18:19:52Z",
      "rollup": {
        "status": "ended",
        "turn_count": 12
      },
      "started_at": "2026-08-13T18:04:11Z"
    }
  ],
  "next_cursor": ""
}
```

Note the two ids. The one `start` printed is `harness_session_id`; the one every
read command takes is `id`. They are different values in different namespaces,
and feeding the printed one to `sessions get` returns a 404. That is a live
defect, not a misunderstanding — [Session ids](./capture.md#session-ids)
explains it and gives the workaround.

Read the session with the `id` from the listing:

```bash
tapesctl sessions get 01JDQ8F3K2M4N6P8R0T2V4X6Z8 --tapes-url http://localhost:8081
```

Every read command prints the server's JSON pretty-printed and nothing else, so
it composes with `jq`.

## Where to go next

- [Capture](./capture.md) — how capture actually works: the two lanes, which
  harness uses which mechanism, what attribution means, and the session-id
  reality.
- [Commands](./commands.md) — the full reference: every command, its flags,
  its environment equivalents, its exit codes and error families.
- [Configuration](./configuration.md) — the precedence chain, `config.toml`,
  and every file `tapesctl` writes.
- [Cassettes](./cassettes.md) — the command surface your deployment serves,
  discovered at runtime.
- [Troubleshooting](./troubleshooting.md) — the failures that actually happen,
  starting with a capture that landed nowhere.

## What tapesctl does not do

- **No telemetry.** `tapesctl` reports nothing about you anywhere. There is no
  variable to set because there is nothing to turn off.
- **No authentication on the read API.** Read commands send no credentials.
- **No server.** `tapesctl` does not run, embed, or start a tapes server, and
  `local up` is the server's verb, not this client's.
