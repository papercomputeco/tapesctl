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
  <https://tapes.dev/docs/>.
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
| `8081` | read API | `sessions`, `traces`, `spans`, `search`, `export`, `seed`, `skills`, `cassettes` |
| `8082` | ingest | `start`, `capture`, `sync` |

Those are the defaults of a local `tapes serve`. A deployment that fronts both
behind one hostname gives you one URL for everything; check with whoever runs
it. What does not vary is which side of the split a command is on.

Read and ingest have independent configuration and local defaults. Use
`--api-url` / `TAPES_API_URL` for reads and `--ingest-url` /
`TAPES_INGEST_URL` for capture. See [Configuration](./configuration.md).

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

The binary lands in `$HOME/.local/bin`, which you own — a normal install never
asks for `sudo`. Because that directory is not on every default `PATH`, the
installer also writes a guarded `PATH` export into your shell's rc file, inside
a sentinel-marked block it rewrites in place rather than duplicating on
re-install. Set `TAPESCTL_INSTALL_DIR` to put it somewhere else.

Supported platforms are Linux and macOS, on x86-64 and arm64.

### Upgrading

```bash
tapesctl upgrade
```

Replaces this binary with the newest published release, or says `already up to
date` and exits successfully when there is nothing to do. The download's
SHA-256 is checked against the published sidecar and the staged file is
sanity-probed before anything replaces the installed binary, so a failed
upgrade leaves the one you had still working. `--version v0.6.0` pins an exact
release; `--nightly` takes the rolling nightly build.

### Uninstalling

```bash
tapesctl uninstall
```

Removes the binary, `~/.tapes`, the cassette cache, and the installer's `PATH`
block — leaving the rest of your rc file untouched. Harness-side capture
plugins are removed separately with `tapesctl plugin uninstall <harness>`.

## Two minutes: capture, then read

Capture a Claude session. The harness behaves as it would unproxied — its
traffic is forwarded to its own provider API — and the capture proxy dies with
it. The URL is the **ingest** port:

```bash
tapesctl start claude --ingest-url http://localhost:8082
```

Before the harness launches, and again when it exits, `tapesctl` prints to
stdout; while the harness holds the terminal it prints nothing at all:

```
capturing · logs at ~/.tapes/logs/start-20260813-180411-54233.log
✓ captured session f47ac10b-58cc-4372-a567-0e02b2c3d479
  pass --web-url for a console link
  logs at ~/.tapes/logs/start-20260813-180411-54233.log
```

Now read it back, against the **read** port:

```bash
tapesctl sessions list --limit 20 --api-url http://localhost:8081
```

```
TITLE                            STATUS     TURNS    COST  LAST ACTIVE  ID
Add a table view                 completed     12   $0.04  5m ago       01a0d365
untitled (f47ac10b)              unknown        —       —  2h ago       01a0d365

2 sessions · more with --cursor eyJzb3J0IjoibGFzdF9…  (full cursor: --json)
```

On a terminal the `ID` column is the leading group of the tapes session id;
`--json` carries the full id, and so does the table when it is piped or the
terminal is wide. The untitled row is a session nothing has derived a title for
yet; the parenthesised value is the leading group of its harness session id.

Note the two ids. The one `start` printed is the `harness_session_id`; the one
every read command takes is the tapes `id`. They are different values in different namespaces,
and feeding the printed one to `sessions get` returns a 404. That is a live
defect, not a misunderstanding — pass the printed id to
`sessions list --harness-id claude --harness-session-id <id>` to resolve it to
the tapes `id`; [Session ids](./capture.md#session-ids) explains the split and
why the filter comes as a pair.

Read the session with the `id` from the listing:

```bash
tapesctl sessions get 01JDQ8F3K2M4N6P8R0T2V4X6Z8 --api-url http://localhost:8081
```

```
Add a table view
01a0d365-2f42-77a1-8473-bd2e295244a4 · claude 2.1.281 · local:jasonwc

Status   completed
Started  Aug 13 18:04 (2h ago), last seen 18:19
Turns    12
Model    claude-opus-5-5
Tokens   1,076 in · 39,288 out
Cost     $0.0421
Cwd      ~/code/tapes

next  tapesctl traces list 01a0d365-2f42-77a1-8473-bd2e295244a4
```

Every read command has a human view like this by default, laid out for the
terminal it is printed on, and `--json` on any of them restores the server's
document so the output still composes with `jq`. The last line of a record
names the command to run next.

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
