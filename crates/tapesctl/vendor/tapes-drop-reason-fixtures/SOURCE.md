# `tapes-drop-reason-fixtures/` — vendoring + sync

`cases/*.json`, `DIGEST` and `README.upstream.md` are a **verbatim copy** of the
shared drop-reason fixture corpus authored in the `tapes` repository. Do not
hand-edit them here — a change belongs upstream, and this copy is refreshed with
`scripts/sync-drop-reason-fixtures.sh`.

Hand-editing is not merely discouraged, it is caught: `DIGEST` seals `cases/`
and is recomputed by this repo's own suite, so an edited case fails here rather
than quietly making a red test green against cases nobody upstream still has.

## Source

* **Repo:** `tapes` (Paper Compute).
* **Path within repo:** `fixtures/drop-reason/`.
* **Current snapshot SHA:** `34f0cf730b1ecb1d2741095632eb95ab4d6f166a`
  ("✨ feat(fixtures): pin the two turn-eligibility gates as agreed policy") —
  the last commit to touch `fixtures/drop-reason`. 14 cases.
  TODO: replace with a tagged fixture-cut id once tapes publishes versioned
  cuts (`fixtures/manifest.json` reserves the `cut` block for exactly that).

The snapshot is the contract: it is pinned to a specific upstream commit, and a
refresh lands in the same PR as whatever consumer change it forces.

## What it pins

Two things, and they are different in kind.

**The vocabulary.** Every answer a capture path can give to "why was this turn
not captured", each classified as **capture policy** — a rule about
capturability that any implementation must share — or **transport/runtime**,
specific to how one deployment moves bytes. Both halves are specified, the
transport half precisely so that "this one is not contract" is recorded rather
than assumed. This client carries the policy reasons whose gates it applies and
does not invent its own spellings for them: the strings are metric label values
and log fields elsewhere, so a reason agreed on but spelled differently is still
two vocabularies.

**The eligibility rules**, executably. Two reasons are pure functions of data a
case can carry, and those are the two that decide whether an exchange is a turn
at all:

| reason | predicate over | pinned by |
| --- | --- | --- |
| `non_turn_request` | `(method, path)` | `cases/non_turn_request.json` `examples` |
| `upstream_status` | the upstream status | `cases/upstream_status.json` `examples` |

Every other reason carries `not_expressible` explaining why it has no examples:
it depends on bytes, streams, reducers or a live upstream, and a case for it
would either restate prose as JSON or pin one implementation's internals.

The policy has two independent implementations in two languages, and capture
fidelity is supposed to be identical whichever path a session took:

| Implementation | Where |
| --- | --- |
| Go, gateway lane | `tapes` `extproc/processor.go` (`isCapturableTurnRequest`, `isCapturableUpstreamStatus`) |
| Go, shared vocabulary | `tapes` `pkg/capture/dropreason.go` (`capture.PolicyDropReasons`) |
| Rust, client (here) | `crates/tapesctl/src/start/turn_policy.rs` |

These two rules were the corpus's recorded divergences until this client grew
them: it had no path or method gate, so anything with a JSON body was a capture
candidate, and it captured a failed exchange with the status in metadata. The
second produced turns the store then refused — an ingest rejection reading
`missing response.message.role`, which is a confusing way to learn that a 400
was never a turn.

## Two things this corpus does not settle

**Which path string the predicate reads.** The rule is the same on both sides;
the input is resolved differently. The gateway is handed the path its proxy
routed. This client resolves the request path against the upstream route first,
because a harness's own path can be a proper suffix of the provider's — a
plan-authenticated Codex asks for `/responses` against a backend base ending in
`/backend-api/codex`. Gating on the unresolved path here would drop every turn
of those sessions while the rule read as identical. See
`turn_policy::provider_path`.

**Precedence.** An exchange can fail both gates at once, and the corpus's README
specifies the order (status first) without asserting it. This client's order is
asserted in `turn_policy`'s own unit tests instead.

## What consumes it here

`crates/tapesctl/tests/drop_reason_corpus.rs` — the client-side oracle. Every
case's `examples` are run against the real predicates, plus the DIGEST seal, a
shape gate, and a one-directional vocabulary check: every reason this client
names must exist in the corpus, be classified `policy`, and be spelled exactly
as the corpus spells it.

The check is one-directional on purpose. This client applies two of the seven
policy gates; the others are drops it makes and does not yet name
(`request_decode` when a body will not decode, an unparseable request that has
no reason here at all), reported as log prose. Asserting the corpus's policy set
against this client's would therefore fail on work not done rather than on
drift. What it does assert is that nothing here is named that the corpus does
not specify, and that adopting a further reason means adopting its spelling.

## How to refresh

```sh
# from a tapes checkout at the target commit
./scripts/sync-drop-reason-fixtures.sh /path/to/tapes

# or detect drift without writing anything
./scripts/sync-drop-reason-fixtures.sh --check /path/to/tapes
```

Then update the snapshot SHA above, run
`cargo test -p tapesctl --test drop_reason_corpus`, and commit the fixture change
with any gate change it forced. The authored home
(`extproc/dropreason_corpus_test.go`) must be green at the same SHA.
