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
* **Current snapshot SHA:** `b5ff59a5f6714c903aaf5278c46ef0031b272db5`
  ("✨ feat(fixtures): specify the drop-reason taxonomy and its boundary
  (#296)") — the last commit to touch `fixtures/content-encoding`. 27 cases.
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

Drift between them is invisible until turns stop being recorded. That failure
has already happened once: this client dropped every
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

**None.** Every case in the corpus holds against this decoder, and the oracle
carries no exemption list.

There was one, and it is worth recording how it ended, because it is the corpus
working exactly as designed. `divergence-empty-body-under-zstd` recorded — as
*observed* upstream, flagged there as a suspected bug rather than promoted to a
rule — that Go's zstd reader returned success with zero bytes for a zero-byte
body while its gzip reader errored on the same input, and its `contested.open`
asked a Rust consumer to report what its binding did rather than assume.

This implementation errors under both codings: libzstd's streaming decoder calls
a zero-byte input an incomplete frame. That answer made the inconsistency
Go-internal rather than a choice between two implementations, which is what
turned the case into the corpus's first genuine cross-language divergence — and
it was resolved where the case itself argued it should be, in the reference
implementation, which now states the empty-body rule above both decoder
libraries so neither can decide it. The case is renamed
`contested-empty-body-under-zstd` and pins the agreed rule.

Nothing here was bent to make it green: this decoder's behaviour is unchanged,
and `an_empty_body_is_an_error_under_both_codings_here` still pins it — now as
the statement the two cases only imply separately, that the outcome does not
depend on which coding the header named. Note also
`contested-empty-body-under-gzip`'s caller precondition, unchanged and still
load-bearing: a bodiless request must never reach the decoder, so neither answer
was reachable in production, which is why the repair was safe.

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
