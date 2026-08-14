# Changelog

Notable changes to `tapesctl`. The project is pre-1.0: minor releases may
break, and every breaking change is recorded here plainly.

## Unreleased

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
