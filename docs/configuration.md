---
title: Configuration
description: How tapesctl resolves a server URL, the config.toml schema, and every file and directory it reads or writes.
sidebar:
  order: 4
---

`tapesctl` has one configurable setting today — the server it talks to — and
one file to hold it. This page covers how that value is resolved, what else
lives beside it, and where diagnostics go.

## Resolving the server URL

Four sources, consulted in this order:

```
1. --tapes-url on the command line   (leaf position beats global position)
2. TAPES_URL in the environment
3. tapes-url in ~/.tapes/config.toml
4. nothing → an error, never a guessed host
```

With none of them:

```
tapesctl: no tapes server URL: pass --tapes-url, set TAPES_URL, or configure a default with `tapesctl config set tapes-url <url>`
```

That refusal is deliberate. A capture pointed at whatever happened to be
listening on a guessed port is worse than one that did not start.

**There is no project-local layer.** No `.tapesrc`, no directory walk, no
per-repository override. One user-level file, one variable, one flag.

Three mechanics that are easy to get wrong:

- **Leaf position beats global position.** `tapesctl --tapes-url A sessions
  list --tapes-url B` uses `B`.
- **The configured value is installed as the flag's default**, rather than
  resolved by hand. So precedence is the argument parser's own, and a default
  does not count as a user-supplied argument — which is why a bare `tapesctl`
  still prints help on a configured machine instead of complaining about a
  missing subcommand.
- **The global `--tapes-url` deliberately carries no environment binding.**
  The parser counts an environment-sourced value as user-supplied, so binding
  `TAPES_URL` at the top level would make a bare `tapesctl` answer `error:
  requires a subcommand` on any machine with the variable exported. The
  per-command declarations carry the binding instead, so the fallback still
  works everywhere it matters.

Cassette discovery resolves the same three sources itself, because it runs
before arguments are parsed. See [Cassettes](./cassettes.md).

### One default, two ports

A tapes deployment serves reads and ingest on separate listeners, and the
configured default holds a single URL. On a machine that both captures and
reads a local server, configuration alone cannot name both.

Configure the one you type least often, and pass the other explicitly:

```bash
tapesctl config set tapes-url http://localhost:8081     # reads, the common case
tapesctl start claude --tapes-url http://localhost:8082 # ingest, when capturing
```

See [The two ports](./introduction.md#the-two-ports) for which command is on
which side.

## config.toml

The path is `~/.tapes/config.toml`, resolved once and nowhere else. Ask for it
rather than assuming:

```bash
tapesctl config path
```

```
/Users/you/.tapes/config.toml
```

`config path` prints the path whether or not the file exists.

The schema is one key:

```toml
# ~/.tapes/config.toml
tapes-url = "http://localhost:8081"
```

| key | type | meaning | validation |
|---|---|---|---|
| `tapes-url` | string | the server every command falls back to | must parse as a URL **and** use scheme `http` or `https` |

Setting it:

```bash
tapesctl config set tapes-url http://localhost:8081
```

Validation happens at write time rather than on every command afterwards, and a
rejected value writes nothing:

```
tapesctl: unknown config key "tapes-erl" (known keys: tapes-url)
tapesctl: invalid tapes URL
tapesctl: tapes-url must be an http or https URL; "ftp" is not a scheme this client can call
```

**The file is deliberately not under `$XDG_CONFIG_HOME`.** It sits beside
`~/.tapes/logs`, `~/.tapes/skills`, and `~/.tapes/codex-app` so there is one
directory to inspect, back up, or delete.

### Rules that are invisible from the help text

- **Unknown keys are preserved, not refused.** Reading ignores them, and
  writing edits the TOML document in place rather than re-serializing it — so
  comments, ordering, your formatting, and keys a newer `tapesctl` wrote all
  survive a `config set`.
- **A malformed file fails only the `config` commands.** They surface a parse
  error; every other command loads with a fallback, warns at `-v`, and
  continues with an empty configuration.
- **`config set` never reads before it writes**, so it can repair a known key
  holding a wrong-typed value. Structurally broken TOML is still refused rather
  than clobbered.
- **`config get` can print nothing from a file that is not empty.** Only keys
  that are both *known* and *set* are listed. A file containing only a key this
  build has never heard of produces empty output — the forward-compatibility
  rule working as designed, and indistinguishable from an empty file.
- **A known-but-unset key prints nothing and exits `0`**, so
  `$(tapesctl config get tapes-url)` is empty rather than an error a script has
  to special-case.

## Logging

One rule governs everything here: **while a harness holds the terminal, nothing
may reach stdout or stderr.** A stray log line lands in the middle of a TUI
frame.

So diagnostics go to a file when, and only when, the command hands over the
terminal *and* verbosity is at its default. In practice that is `start` without
`-v`. Every other command — `sync`, `capture`, the read commands — logs to
stderr as usual.

```
~/.tapes/logs/start-YYYYMMDD-HHMMSS-<pid>.log
```

Files are created `0600` and appended to, never truncated. The path is printed
before the harness launches and again when it exits.

Pass `-v` to `start` to stream to stderr instead of a file, accepting what that
does to the display. That is the documented way to watch a capture live.

Level precedence is `RUST_LOG`, then the `-v` count, then `info`. A set-but-empty
`RUST_LOG` is treated as unset. An unparseable one prints
`tapesctl: ignoring invalid RUST_LOG <directive> (<err>)` to stderr and falls
back.

**There is no stderr fallback when the log file cannot be opened.** The run
prints `tapesctl: diagnostics disabled — no log file (<err>)` once, then
discards events. A corrupted TUI is judged more costly than a lost debugging
session.

## Files and directories

What `tapesctl` writes:

| path | written by |
|---|---|
| `~/.tapes/config.toml` | `config set` |
| `~/.tapes/logs/start-*.log` | `start`, at default verbosity |
| `~/.tapes/skills/<name>.md` | `skill generate` |
| `~/.tapes/codex-app/handoff.json` | `plugin install codex-app` |
| `~/.tapes/codex-app/plugin/` | `plugin install codex-app` |
| `~/.agents/skills/`, `./.agents/skills/`, `~/.claude/skills/`, `./.claude/skills/` | `skill sync` |
| `~/.pi/agent/extensions/tapes-gateway.ts` | `plugin install pi` |
| `~/.config/opencode/plugins/tapes-gateway.ts` | `plugin install opencode` |
| `~/.codex/config.toml` (or `$CODEX_HOME/config.toml`) | `plugin install`/`uninstall codex-app`, patched in place |
| `<platform cache>/tapesctl/cassettes/<key>.json` | any command, when cassette discovery runs |

Skill documents, log files, and installed plugin files are written `0600`.

What it reads but never writes:

| path | read by |
|---|---|
| `~/.claude/projects/` | the transcript tailer and `sync` |
| `~/.claude/sessions/<pid>.json` | Claude attribution |
| `$CODEX_HOME/sessions`, or `~/.codex/sessions` | Codex attribution |

## Environment variables

| variable | read by |
|---|---|
| `TAPES_URL` | `start`, `capture`, `sync`, every read command, cassette discovery |
| `TAPES_UPSTREAM` | `start`, `capture` |
| `TAPES_WEB_URL` | `start`, `capture` |
| `TAPES_ORG_ID` | `start`, `capture` |
| `TAPES_AUTH_SUBJECT` | `start`, `capture`, `sync` |
| `RUST_LOG` | logging, all commands |
| `TAPESCTL_CACHE_DIR` | the cassette surface cache |
| `CODEX_HOME` | `plugin install`/`uninstall codex-app`, `capture codex-app`, `start codex` |
| `USER`, then `USERNAME` | the default `--auth-subject`: `local:<user>`, else `local:unknown` |
| `OPENAI_API_KEY` | `start codex` upstream selection; `skill generate` |
| `ANTHROPIC_API_KEY` | `skill generate` |

There is no telemetry variable, because there is no telemetry. `tapesctl`
reports nothing about you anywhere.

**The parent environment is inherited wholesale by a launched harness.**
`start` clears nothing, so a variable set in your shell reaches the harness
unchanged.

## Commands that ignore `--tapes-url`

Because the global flag propagates into every leaf's help, `--tapes-url` is
rendered for commands that never make an HTTP call: `config set`, `config get`,
`config path`, `skill list`, `skill sync`, `version`, and `plugin uninstall`.
It is inert in all of them.

The reverse is worth stating too: `plugin install` and `plugin uninstall` write
local files from bytes the binary already carries and fetch nothing; `skill
sync` is a pure local copy; `skill list` reads a directory.
