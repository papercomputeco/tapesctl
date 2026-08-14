---
title: Cassettes
description: The command surface your tapes deployment serves, discovered at runtime — how it is generated, how it is cached, and how to call it.
sidebar:
  order: 5
---

A tapes deployment can serve **cassettes**: independently built API extensions
mounted under `/v1/cassettes/<name>`. `tapesctl` discovers whichever ones your
server serves and mounts them as commands, so the same binary gets a correct
command line for a cassette it has never heard of.

Cassette commands talk to the **read API** (`8081` on a local `tapes serve`),
and discovery uses the same server URL as everything else. Deploying and
configuring cassettes is an operator task, documented with the server at
<https://tapes.dev/docs/cassettes/>.

## Listing what a server serves

The noun's help *is* the listing:

```bash
tapesctl cassettes --help --tapes-url http://127.0.0.1:8899
```

```
Call cassette methods served by your tapes deployment.

Cassettes are API extensions your deployment serves under /v1/cassettes; their
commands are discovered from the server at runtime, so the set listed here is
whatever your deployment actually serves — not a list compiled into this binary.
...

Usage: tapesctl cassettes [OPTIONS] <COMMAND>

Commands:
  hello-world  A demo cassette
  help         Print this message or the help of the given subcommand(s)
```

Given a server but no subcommand, `tapesctl cassettes` reports the discovered
set as the subcommands it wanted and exits `2`:

```
error: 'tapesctl cassettes' requires a subcommand but one was not provided
  [subcommands: hello-world, help]
```

Then one cassette's methods:

```bash
tapesctl cassettes hello-world --help --tapes-url http://127.0.0.1:8899
```

```
A demo cassette

Usage: tapesctl cassettes hello-world [OPTIONS] <COMMAND>

Commands:
  create-hello  Record a greeting
  get-hello     Greet someone
```

And one method:

```bash
tapesctl cassettes hello-world get-hello --help --tapes-url http://127.0.0.1:8899
```

```
Greet someone

Usage: tapesctl cassettes hello-world get-hello [OPTIONS] <WHO>

Arguments:
  <WHO>  Who to greet

Options:
      --loud <VALUE>     Shout it
  -h, --help             Print help

Calls GET /v1/cassettes/hello-world/hello/{who}
```

**The canonical spelling is `tapesctl cassettes <name> <method>`.** Use that
form everywhere.

## Calling a method

```bash
tapesctl cassettes hello-world get-hello world --tapes-url http://127.0.0.1:8899
```

```json
{
  "greeting": "hello",
  "path": "/v1/cassettes/hello-world/hello/world"
}
```

Query parameters are flags:

```bash
tapesctl cassettes hello-world get-hello world --loud yes --tapes-url http://127.0.0.1:8899
```

```json
{
  "greeting": "hello",
  "path": "/v1/cassettes/hello-world/hello/world?loud=yes"
}
```

A request body is `--body`, inline or from a file with `@`:

```bash
tapesctl cassettes hello-world create-hello --body '{"hello":"hi"}' --tapes-url http://127.0.0.1:8899
tapesctl cassettes hello-world create-hello --body @row.json --tapes-url http://127.0.0.1:8899
```

Both send the same request. The body is validated before anything is sent:

```
tapesctl: --body is not valid JSON
```

## How a command is generated

Each command comes from the cassette's own OpenAPI document, republished by the
server onto the paths a client can actually call.

- An `operationId` becomes a kebab-case method name — `getHello` is
  `get-hello`.
- A **path** parameter becomes a required positional, its value name
  uppercased.
- A **query** or **header** parameter becomes `--<name> <VALUE>`, required only
  if the spec says so.
- A request body becomes `--body <JSON>`, required only if the spec says so,
  and accepts `@<path>` to read from a file.
- Every method's help ends by naming the route it calls. That is the one piece
  of context you cannot recover from the command name, and it is what makes a
  generated surface auditable.

Four flag names are reserved and can never be handed to a cassette parameter:
`tapes-url`, `body`, `help`, and `verbose`.

One collision is resolved by skipping the cassette rather than failing: a
cassette **named `help`** is silently skipped, because a duplicate would crash
the parser and a deployment's choice of name must not crash someone's CLI. A
cassette named after a **built-in** needs no special case any more — with the
whole surface under the `cassettes` noun, `tapesctl cassettes sessions` and
`tapesctl sessions` are different commands, and a server cannot redefine what
the second one means.

### Why discovery is a runtime step

Which cassettes exist is deployment configuration: an operator lists cassette
OpenAPI URLs and the server admits them at runtime. Nothing about the set is
known when the server is built, let alone when this client is. `tapesctl` ships
as a prebuilt binary, so a compiled-in list would be one deployment's cassettes
frozen into every user's install — and the people most likely to run a custom
cassette are exactly the ones a stale list would fail.

## Discovery never fails

Discovery runs before your arguments are parsed, resolving the server from the
same three sources as everything else — but only when the command line can
actually reach the generated surface: `tapesctl cassettes …`, `tapesctl help
…`, or a bare / flags-only invocation whose help must describe the noun. Every
other command builds its command tree with no discovery at all — no cache
read, no network. When discovery does run, every failure mode degrades instead
of raising:

- no server configured,
- a URL that does not parse,
- a server that cannot be reached,
- a document that does not decode.

Each costs the cassette nouns and nothing else. The hand-written surface keeps
working on a machine that cannot reach any tapes server at all:

```bash
tapesctl version --tapes-url http://127.0.0.1:9
```

```
tapesctl 0.1.0
All in all, just another tape in the stereo
```

Bare `tapesctl cassettes` is never an unknown-command error, even on a machine
that has never seen a server — the noun is always mounted. It prints its help
and exits `2`.

## The cache

A discovered surface is cached per server, so `--help` stays instant and keeps
working offline:

```
<platform cache dir>/tapesctl/cassettes/<sanitised-base>-<hash>.json
```

For example, `http___127_0_0_1_8899_-c48d02e05b9a7bb5.json`.

The cache is revalidated after ten minutes. Cassette sets change when an
operator redeploys, which is rare next to how often a CLI runs, so ten minutes
keeps a working session fast while still picking up a new cassette without
anyone clearing anything.

Recently discovered commands keep working from the cache while the server is
unreachable — a cached surface survives the server going down mid-session.

`TAPESCTL_CACHE_DIR` overrides the location, and when set, files are written
directly into it rather than into a `tapesctl/cassettes` subdirectory. It is
useful for pinning the location in CI.

## What the top-level help tells you

The epilogue on `tapesctl --help` changes with what discovery found, so the
help itself distinguishes "no server" from "no cassettes":

**No server configured:**

```
Cassette commands are served by your tapes deployment, not built into this binary: they
are discovered from the server and mounted under `tapesctl cassettes`.
No server is configured, so none are listed; pass --tapes-url, set TAPES_URL, or
run `tapesctl config set tapes-url <url>` to see them from here on.
```

**A server, serving no cassettes:**

```
Cassette commands are served by your tapes deployment, not built into this binary: they
are discovered from the server and mounted under `tapesctl cassettes`.
No cassettes were discovered from http://127.0.0.1:8900, so none are listed; re-run with -v for why.
```

**A server with cassettes** — the explanation stops once there is something to
list:

```
Cassette commands are served by your tapes deployment, not built into this binary: they
are discovered from the server and mounted under `tapesctl cassettes`.
Run `tapesctl cassettes` to list them.
```

The "re-run with `-v` for why" is literal: an operator's typo in a cassette URL
is otherwise indistinguishable from the cassette not existing, so the discovery
document carries the problems and `-v` prints them.

Because the listing comes from a server, `tapesctl cassettes` on a machine that
names none lists nothing at all. That is the strongest reason to run
[`tapesctl config set tapes-url`](./configuration.md) once.

## The older spelling

Cassettes used to mount as top-level nouns — `tapesctl <name> <method>`. That
spelling has been removed: it shipped one release as a hidden alias (parsing
but unlisted, so nothing taught it to anyone new) and now fails like any other
unknown command. A script still typing it gets clap's normal error; the fix is
mechanical — insert `cassettes` before the name:

```console
$ tapesctl hello-world get-hello        # old, now an error
$ tapesctl cassettes hello-world get-hello
```

Retiring the aliases is what bought the startup behavior described above: when
any first token could have been a cassette, every invocation had to run
discovery just to build its command tree. With the surface confined to the
`cassettes` noun, everything else skips discovery entirely.
