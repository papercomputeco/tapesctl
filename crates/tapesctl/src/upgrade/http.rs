//! HTTP-facing mechanics for `tapesctl upgrade`: the timeout-configured
//! client, target resolution against the release bucket, and the
//! streamed/capped object transfers.
//!
//! Split from [`super::artifact`], which owns the local file mechanics
//! (staging, digest verification, probing, the swap). Every function upholds
//! the pipeline's abort contract: a failure leaves the installed binary
//! byte-identical.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use reqwest::Client;
use snafu::{OptionExt, ResultExt};
use versions::Versioning;

use super::artifact::UpgradeTarget;
use super::{LATEST_VERSION_PATH, UpgradeError};

/// Connect timeout for every bucket request. The bucket is a CDN, and a connect
/// that has not completed within 2 s is down, not slow.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Overall per-request timeout for the small metadata objects — the
/// latest-version lookup and the `.sha256` digest, both well under a kilobyte.
const METADATA_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-read stall timeout, applied client-wide. The binary download
/// deliberately carries NO overall timeout — artifact size and link speed vary
/// too much for one number to be honest, and a hard cap would false-abort big
/// downloads on slow links. A stalled transfer is caught here instead: a
/// connection that delivers no bytes for this long is dead, while a
/// slow-but-moving multi-minute download never trips it.
const READ_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the latest-version response body. A version label is a handful of
/// bytes; anything larger is not one.
const MAX_VERSION_BODY_BYTES: u64 = 128;

/// Hops a bucket redirect may take before the transfer is refused.
const MAX_REDIRECTS: usize = 5;

/// Sanity ceiling on the downloaded artifact.
///
/// The transfer carries no overall *timeout* by design (see
/// [`READ_STALL_TIMEOUT`]), which leaves size as the only bound — without one,
/// a server streaming indefinitely fills the filesystem the install directory
/// lives on, usually `/` or `$HOME`. Well past any honest artifact: the
/// released binary is tens of megabytes.
const MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// Cap on the `.sha256` response body. sha256sum format is ~75 bytes; 1 KiB is
/// generous headroom while still refusing to buffer a misconfigured endpoint's
/// unbounded junk.
const MAX_CHECKSUM_BODY_BYTES: u64 = 1024;

/// Build the HTTP client the pipeline uses for every bucket request, with the
/// connect and per-read stall timeouts baked in (see [`CONNECT_TIMEOUT`] /
/// [`READ_STALL_TIMEOUT`]).
pub(super) fn build_client() -> Result<Client, UpgradeError> {
    use super::upgrade_error::*;

    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_STALL_TIMEOUT)
        // Redirects are followed only while they stay on https. reqwest's
        // default policy would happily follow https → http, and the digest
        // gate is no defense there: anything able to redirect the binary can
        // redirect the `.sha256` beside it, since both travel this same
        // client. The rest of this crate already refuses redirects outright on
        // the tapes API for a related reason — a silently followed redirect
        // destroys the diagnosis — so downgrading the transport here would be
        // the odd one out.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" {
                attempt.error("refusing to follow a redirect off https")
            } else if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
        .build()
        .context(BuildHttpClientSnafu)
}

/// Read at most `cap` bytes of `response`'s body; `Ok(None)` when the declared
/// or streamed length exceeds the cap.
async fn read_body_capped(
    mut response: reqwest::Response,
    cap: u64,
) -> Result<Option<Vec<u8>>, reqwest::Error> {
    if response.content_length().is_some_and(|len| len > cap) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        // Checked before appending: growing past the cap and then noticing
        // would buffer one whole frame beyond what the cap permits.
        if body.len() as u64 + chunk.len() as u64 > cap {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// What resolving an [`UpgradeTarget`] against the bucket decided: the install
/// is already current, or the pipeline should download from `prefix` and
/// announce `version` on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resolution {
    /// The installed version already satisfies the target.
    AlreadyCurrent {
        /// The version that is both installed and targeted.
        version: String,
    },
    /// A download is warranted.
    Download {
        /// Bucket prefix to fetch from (`latest`, `nightly`, `v<version>`).
        prefix: String,
        /// Version label to report as the upgrade's destination.
        version: String,
    },
}

/// Resolve `target` against the bucket at `base_url`.
///
/// `Latest` fetches the plain-text `latest/version` object and compares it with
/// `current_version`; `Pinned` compares without any network call; `Nightly`
/// always resolves to a download because nightly builds carry no orderable
/// version.
pub(super) async fn resolve_target(
    client: &Client,
    base_url: &str,
    current_version: &str,
    target: &UpgradeTarget,
) -> Result<Resolution, UpgradeError> {
    use super::upgrade_error::*;

    let current = parse_comparable_version(current_version);
    match target {
        UpgradeTarget::Nightly => Ok(Resolution::Download {
            prefix: target.bucket_prefix(),
            version: "nightly".to_owned(),
        }),
        UpgradeTarget::Pinned(version) => {
            if current.as_ref() == Some(version) {
                // `v`-prefixed like the Latest arm's raw body, so the same
                // state does not print two different spellings depending on
                // how the user asked for it.
                Ok(Resolution::AlreadyCurrent {
                    version: format!("v{version}"),
                })
            } else {
                Ok(Resolution::Download {
                    prefix: target.bucket_prefix(),
                    version: version.to_string(),
                })
            }
        }
        UpgradeTarget::Latest => {
            let url = format!("{}/{LATEST_VERSION_PATH}", base_url.trim_end_matches('/'));
            let response = client
                .get(&url)
                .timeout(METADATA_TIMEOUT)
                .send()
                .await
                .context(FetchLatestVersionSnafu { url: &url })?;
            snafu::ensure!(
                response.status().is_success(),
                LatestVersionStatusSnafu {
                    url: &url,
                    status: response.status(),
                }
            );
            // Capped read: a version object is a handful of bytes, so a bigger
            // body is not a version — never buffer it whole.
            let body = read_body_capped(response, MAX_VERSION_BODY_BYTES)
                .await
                .context(FetchLatestVersionSnafu { url: &url })?
                .context(MalformedLatestVersionSnafu {
                    value: format!("(body larger than {MAX_VERSION_BODY_BYTES} bytes)"),
                })?;
            let raw = String::from_utf8_lossy(&body).trim().to_owned();
            let latest = parse_comparable_version(&raw)
                .context(MalformedLatestVersionSnafu { value: &raw })?;
            if current == Some(latest) {
                Ok(Resolution::AlreadyCurrent { version: raw })
            } else {
                Ok(Resolution::Download {
                    prefix: target.bucket_prefix(),
                    version: raw,
                })
            }
        }
    }
}

/// Parse a version label for comparison: trimmed, optional leading `v`, and a
/// required numeric first component.
///
/// `None` for the labels that identify no orderable version — an unstamped
/// build, a nightly, junk. A `None` on the *installed* side means version
/// comparison cannot rule the download out, so the pipeline downloads, which is
/// the right answer for both a dev build and a nightly.
///
/// The name is split from its build metadata before the sentinel check, because
/// what a stamped build reports is `nightly+3f2a1b9`, not a bare `nightly` — the
/// commit is what distinguishes one nightly from the next, so it is always
/// there. Without the split the sentinel would never match and the answer would
/// fall through to whether `versions` happens to reject the whole string.
pub(super) fn parse_comparable_version(label: &str) -> Option<Versioning> {
    let trimmed = label.trim();
    let name = trimmed.split('+').next().unwrap_or(trimmed);
    if matches!(name, "" | "dev" | "nightly") {
        return None;
    }
    Versioning::new(trimmed.strip_prefix('v').unwrap_or(trimmed)).filter(|v| v.nth(0).is_some())
}

/// Stream the binary at `url` into the staging file.
///
/// The staging file is created non-executable: execute permission is only
/// granted after the digest verifies, so a half-written or tampered download is
/// never a runnable file.
pub(super) async fn download_to_staging(
    client: &Client,
    url: &str,
    staging: &Path,
) -> Result<(), UpgradeError> {
    use super::upgrade_error::*;

    let mut response = client
        .get(url)
        .send()
        .await
        .context(DownloadSnafu { url })?;
    snafu::ensure!(
        response.status().is_success(),
        DownloadStatusSnafu {
            url,
            status: response.status(),
        }
    );
    // `create_new` + 0o600, not `File::create`, for three reasons that all bite
    // the same file:
    //
    //  * `create_new` fails rather than following a symlink planted at the
    //    staging path, which would otherwise send the download to the link's
    //    target and hand the subsequent chmod to that target instead.
    //  * It also fails when a concurrent upgrade already owns the staging
    //    name — two runs sharing one fixed path would otherwise interleave,
    //    letting one verify bytes the other has since replaced.
    //  * 0o600 keeps the partially-downloaded, not-yet-verified bytes
    //    unreadable by anyone else and unexecutable by everyone, closing the
    //    window that `File::create`'s truncate-in-place left open when a
    //    previous run died between chmod and rename.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(staging)
        .context(WriteStagingSnafu { path: staging })?;
    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await.context(DownloadSnafu { url })? {
        written += chunk.len() as u64;
        snafu::ensure!(
            written <= MAX_ARTIFACT_BYTES,
            ArtifactTooLargeSnafu {
                url,
                cap: MAX_ARTIFACT_BYTES
            }
        );
        file.write_all(&chunk)
            .context(WriteStagingSnafu { path: staging })?;
    }
    Ok(())
}

/// Fetch the published `.sha256` object at `url` and return the expected digest
/// as lowercase hex.
///
/// The object body is sha256sum format (`<hex>  <filename>`); only the digest
/// field is used. A 404 is [`UpgradeError::MissingChecksum`] — an absent
/// checksum aborts the upgrade rather than skipping verification.
pub(super) async fn fetch_expected_digest(
    client: &Client,
    url: &str,
) -> Result<String, UpgradeError> {
    use super::upgrade_error::*;

    let response = client
        .get(url)
        .timeout(METADATA_TIMEOUT)
        .send()
        .await
        .context(DownloadSnafu { url })?;
    // Any non-success answer for the sidecar is treated as "no publishable
    // checksum here", not just 404: an S3-compatible bucket without
    // `ListBucket` answers 403 for a missing object, and a gateway may answer
    // 5xx. All of them mean the same thing to us — we cannot verify — and the
    // message that says so is the one worth printing.
    snafu::ensure!(
        response.status().is_success(),
        MissingChecksumSnafu {
            url,
            status: response.status()
        }
    );
    // Capped read: a sha256sum line is under 100 bytes, so a bigger body is not
    // a checksum — never buffer it whole.
    let body = read_body_capped(response, MAX_CHECKSUM_BODY_BYTES)
        .await
        .context(DownloadSnafu { url })?
        .context(MalformedChecksumSnafu {
            value: format!("(body larger than {MAX_CHECKSUM_BODY_BYTES} bytes)"),
        })?;
    let body = String::from_utf8_lossy(&body).into_owned();
    let digest = body
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    snafu::ensure!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        MalformedChecksumSnafu { value: body.trim() }
    );
    Ok(digest)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_stamped_release_compares_equal_to_the_bare_tag() {
        // The bucket publishes `v0.7.0`; the binary reports `v0.7.0+3f2a1b9`.
        // Build metadata does not affect precedence, so these are the same
        // release — which is what makes "already up to date" work at all.
        assert_eq!(
            parse_comparable_version("v0.7.0+3f2a1b9"),
            parse_comparable_version("v0.7.0")
        );
    }

    #[test]
    fn the_labels_that_name_no_release_are_unorderable() {
        // Each of these means "comparison cannot rule the download out". The
        // stamped forms carry a commit, which is exactly why the sentinel check
        // has to look past the build metadata.
        for label in ["", "dev", "nightly", "nightly+3f2a1b9", "not-a-version"] {
            assert!(
                parse_comparable_version(label).is_none(),
                "{label:?} should not be orderable"
            );
        }
    }

    #[test]
    fn an_unstamped_development_build_is_orderable_but_never_current() {
        // `0.0.0-dev+sha` parses — it is a prerelease of 0.0.0 — and that is
        // fine: it compares unequal to every real release, so a developer
        // running `upgrade` downloads rather than being told they are current.
        let dev = parse_comparable_version("0.0.0-dev+3f2a1b9").unwrap();
        assert_ne!(Some(dev), parse_comparable_version("v0.7.0"));
    }

    #[tokio::test]
    async fn a_pinned_target_matching_the_install_needs_no_network() {
        // The client points nowhere reachable on purpose: resolving a pinned
        // target that equals the installed version must not touch the network.
        let client = build_client().unwrap();
        let target = UpgradeTarget::parse("v0.7.0").unwrap();

        let resolution = resolve_target(&client, "http://127.0.0.1:1", "v0.7.0+abc1234", &target)
            .await
            .unwrap();

        assert_eq!(
            resolution,
            Resolution::AlreadyCurrent {
                version: "v0.7.0".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn a_pinned_downgrade_resolves_to_that_version_s_prefix() {
        let client = build_client().unwrap();
        let target = UpgradeTarget::parse("v0.6.0").unwrap();

        let resolution = resolve_target(&client, "http://127.0.0.1:1", "v0.7.0+abc1234", &target)
            .await
            .unwrap();

        // Direction-agnostic: a bad release needs an escape hatch.
        assert_eq!(
            resolution,
            Resolution::Download {
                prefix: "v0.6.0".to_owned(),
                version: "0.6.0".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn nightly_always_downloads_without_asking_the_bucket() {
        let client = build_client().unwrap();

        let resolution = resolve_target(
            &client,
            "http://127.0.0.1:1",
            "nightly+abc1234",
            &UpgradeTarget::Nightly,
        )
        .await
        .unwrap();

        assert_eq!(
            resolution,
            Resolution::Download {
                prefix: "nightly".to_owned(),
                version: "nightly".to_owned()
            }
        );
    }
}
