# `tapes-content-encoding-fixtures/` — vendoring + sync

`cases/*.json`, `DIGEST` and `README.upstream.md` are a **verbatim copy** of the
shared content-encoding fixture corpus authored in the `tapes` repository. Do
not hand-edit them here — a change belongs upstream, and this copy is refreshed
with `scripts/sync-content-encoding-fixtures.sh`.

Hand-editing is not merely discouraged, it is caught: `DIGEST` seals `cases/`
and is recomputed by this repo's own suite, so an edited case fails here rather
than quietly making a red test green against cases nobody upstream still has.

## Source

* **Repo:** `tapes` (Paper Compute).
* **Path within repo:** `fixtures/content-encoding/`.
* **Current snapshot SHA:** `2c4607c87153342375bc0a67409290b3879ca866`
  ("✨ feat(fixtures): pin that a concatenated stream decodes past its first
  member") — the last commit to touch `fixtures/content-encoding`. 27 cases.
  TODO: replace with a tagged fixture-cut id once tapes publishes versioned
  cuts (`fixtures/manifest.json` reserves the `cut` block for exactly that).

The snapshot is the contract: it is pinned to a specific upstream commit, and a
refresh lands in the same PR as whatever consumer change it forces.

## What it pins

`(body bytes, Content-Encoding value)` → decoded bytes, a reported salvage, or a
classified error. Not a header contract but a **capture policy**: which codings
are readable, how stacked layers compose (peeled right-to-left), that a
concatenated multi-member stream is read to its end rather than to its first
member, the 32 MiB per-layer output cap, the zstd window bound, and the
two-conjunct salvage rule.

The policy has two independent implementations in two languages, and capture
fidelity is supposed to be identical whichever path a session took:

| Implementation | Where |
| --- | --- |
| Go, reference | `tapes` `pkg/capture/contentencoding.go` |
| Go, gateway lane | `tapes-extproc` — calls the same function, deliberately |
| Rust, client (here) | `crates/tapesctl/src/start/content_encoding.rs` |

Drift between them is invisible until turns stop being recorded. PCC-1126 is
that failure already having happened: this client dropped every
`content-encoding: zstd` request body — all of Codex/pi's traffic — while the
gateway route decoded the same bytes fine, and nothing was red, because each
side only had its own tests.

## What consumes it here

`crates/tapesctl/tests/content_encoding_corpus.rs` — the client-side oracle.
Every case is built (a `build` recipe is compressed locally; `bytes_b64` is
taken literally), run through `decode_content_encoding`, and asserted against
its declared outcome. Plus the DIGEST seal and a shape gate.

`expect.error.detail` and `contested` are read but never asserted: both are
prose for the reader, and asserting either would pin one implementation's
phrasing as the contract.

## Known divergence

**`divergence-empty-body-under-zstd` does not hold here, and is exempted by
name** in the oracle. The case records — as *observed* upstream, flagged there
as a suspected bug rather than promoted to a rule — that Go's zstd reader
returns success with zero bytes for a zero-byte body while its gzip reader
errors on the same input. Its `contested.open` asks a Rust consumer to report
what its binding does rather than assume.

The answer: **this implementation errors under both codings.** libzstd's
streaming decoder calls a zero-byte input an incomplete frame. So the
inconsistency upstream describes is Go-internal, and does not reproduce here —
the two codings already agree in this implementation, on the answer the case's
own `contested` block argues is the right one.

That makes it the corpus's first genuine cross-language divergence, and the
place to resolve it is the reference implementation (make Go's two codings
agree), not this consumer. Teaching either decoder to return empty-for-empty in
order to make a test green would silently swallow a body lost in flight, which
is the failure the whole corpus exists to make loud. Note also
`contested-empty-body-under-gzip`'s caller precondition: with a bodiless request
never reaching the decoder, neither answer is reachable in production.

The exemption is bounded rather than a mute. `an_empty_body_is_an_error_under_both_codings_here`
pins what this implementation actually does, so it goes red if this decoder ever
starts accepting an empty body, and the exemption itself goes stale the moment
upstream changes or renames the case. When upstream resolves the pair, that test
changes, the exemption is deleted, and the case rejoins the oracle.

## How to refresh

```sh
# from a tapes checkout at the target commit
./scripts/sync-content-encoding-fixtures.sh /path/to/tapes

# or detect drift without writing anything
./scripts/sync-content-encoding-fixtures.sh --check /path/to/tapes
```

Then update the snapshot SHA above, run
`cargo test -p tapesctl --test content_encoding_corpus`, and commit the fixture
change with any decoder change it forced. The authored home
(`pkg/capture/contentencoding_corpus_test.go`) must be green at the same SHA.
