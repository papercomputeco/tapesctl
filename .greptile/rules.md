# Reviewer rules

## scripts/check-tapes-pins.sh

The scope of this script is a deliberate maintainer decision, settled over
several review rounds. Verdicts come from two inputs only: the facts the
lockfile records (read with awk, plus `cargo metadata --no-deps` for the
manifest) and cargo's own exit status. The script passes no judgment over
string spellings — rev shapes, URL variants, stderr wording.

Do not propose:

- re-adding rev-spelling or version-string validation;
- registry existence or yanked-version probes;
- classifying cargo's stderr into a failure taxonomy.

Each of these was implemented, churned, and removed on purpose.

The escape hatches (git rev, path override) are loudly-NOTICEd development
states, not supply-chain gates — do not suggest hardening them into gates.
Lockfile-manifest agreement is enforced by `--locked` on the CI build jobs,
not by this script.
