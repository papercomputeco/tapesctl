//! Decoding a captured body's `Content-Encoding`.
//!
//! This is the client-side half of a contract whose other half lives in the
//! tapes repository, in `pkg/capture/contentencoding.go` — which is itself a
//! deliberate mirror of what the extproc capture adapter does at the AI
//! Gateway. All three must agree, because capture fidelity is supposed to be
//! identical whether a session was captured through Paper's cloud or through
//! `tapesctl start`, and a body that one path can read and the other cannot is
//! that promise broken in the least visible way possible: the turn is forwarded,
//! the harness answers, and nothing is recorded.
//!
//! That is not hypothetical. Before this decode step existed the proxy handed
//! compressed bytes straight to a JSON parser. pi's
//! `openai-codex` provider sends `content-encoding: zstd`, which meant every
//! turn of a `tapesctl start pi` session was dropped while the cloud route
//! stored the same traffic decoded and intact.
//!
//! The rules below are the Go implementation's rules, restated rather than
//! reinvented — the encodings it accepts, the right-to-left layer peeling, the
//! size cap, and the salvage-on-truncation behaviour. The failure mode of a
//! capture contract implemented twice is that the copies drift while both stay
//! green, so where this cannot match Go it says so out loud rather than quietly
//! choosing differently.
//!
//! Saying so is no longer only in prose. The policy has a written specification
//! — the shared fixture corpus authored in tapes at `fixtures/content-encoding/`
//! — which both implementations table-test against, and which is vendored here
//! at `vendor/tapes-content-encoding-fixtures/` and run by
//! `tests/content_encoding_corpus.rs`. The unit tests below are still the
//! readable statement of intent; the corpus is what makes a rule changed on one
//! side turn the other side red. A behaviour change here belongs upstream in
//! the corpus first.
//!
//! Decoding is for *capture only*. The proxy forwards the request body exactly
//! as it arrived, encoding header included; nothing here touches the bytes that
//! go upstream.

use std::borrow::Cow;
use std::io::Read;

use snafu::Snafu;

/// Caps one decoded body, per layer. Bounds a decompression bomb: the encoded
/// bytes are already capped on the way in by the request peek, but a few KiB of
/// zstd expands to gigabytes if nothing stops it.
///
/// 32 MiB is the Go side's `capture.MaxDecompressedBytes`, which in turn matches
/// extproc's. It has to: a body the gateway accepted and stored must not be one
/// this client refuses, or the two capture paths would disagree about which
/// turns exist.
pub const MAX_DECODED_BYTES: usize = 32 << 20;

/// Window bound handed to the zstd decoder, as a base-2 log of
/// [`MAX_DECODED_BYTES`].
///
/// Go passes both `WithDecoderMaxWindow` and `WithDecoderMaxMemory`; the Rust
/// binding exposes the window only. The output cap is what actually bounds the
/// bomb in both implementations — this is the cheaper guard that refuses a
/// hostile frame before it is expanded rather than after.
const ZSTD_WINDOW_LOG_MAX: u32 = MAX_DECODED_BYTES.trailing_zeros();

/// A decoded body, plus what had to be tolerated to produce it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBody<'a> {
    /// The decoded bytes. Borrowed — not copied — when there was nothing to
    /// decode, which is the common case for an uncompressed harness.
    pub bytes: Cow<'a, [u8]>,

    /// Set when the body was recovered from a stream that ended early, so the
    /// decode succeeded only because partial output was accepted. Surfaced
    /// rather than swallowed so a salvaged capture is never silently
    /// indistinguishable from a clean one.
    pub truncated: bool,
}

/// Why a body could not be decoded.
///
/// Every variant names the full header as well as the failing layer: with
/// stacked encodings the layer alone does not say which header produced it.
#[derive(Debug, Snafu)]
pub enum DecodeError {
    /// An encoding this build has no decoder for. `deflate` and `br` land here
    /// deliberately — no capture path emits them, and a decoder for an encoding
    /// nothing produces is untested code that only exists to be wrong later.
    #[snafu(display("content-encoding {header:?}: unsupported encoding {layer:?}"))]
    Unsupported { header: String, layer: String },

    /// The decoded body crossed [`MAX_DECODED_BYTES`].
    #[snafu(display(
        "content-encoding {header:?}: {layer} decoded body exceeds {MAX_DECODED_BYTES} bytes"
    ))]
    TooLarge { header: String, layer: &'static str },

    /// The stream was corrupt, or ended early without having produced anything.
    #[snafu(display("content-encoding {header:?}: {layer} read: {source}"))]
    Read {
        header: String,
        layer: &'static str,
        source: std::io::Error,
    },
}

/// Decode `body` according to `encoding`, for handing to a parser.
///
/// An unrecognized encoding is an error rather than a pass-through. Handing
/// compressed bytes to a parser that expects JSON yields a failure well away
/// from the actual cause — which is exactly how this went unnoticed — and
/// naming the real problem here costs nothing, because the caller drops the
/// turn either way.
///
/// Truncated streams are salvaged rather than refused when they yielded any
/// output at all, and the salvage is reported in [`DecodedBody::truncated`]. A
/// capture that lost its tail is still most of a turn; refusing it would discard
/// everything the stream did deliver in exchange for nothing.
pub fn decode_content_encoding<'a>(
    body: &'a [u8],
    encoding: Option<&str>,
) -> Result<DecodedBody<'a>, DecodeError> {
    let header = encoding.unwrap_or_default();
    let normalized = header.trim().to_ascii_lowercase();

    // Layers are peeled right-to-left: RFC 9110 §8.4 lists encodings in the
    // order they were applied, so the last one listed is the outermost and
    // comes off first.
    let layers = split_content_encoding(&normalized);
    let mut current = Cow::Borrowed(body);
    let mut truncated = false;
    for layer in layers.iter().rev() {
        let (decoded, layer_truncated) = decode_one_layer(&current, layer, header)?;
        truncated = truncated || layer_truncated;
        current = Cow::Owned(decoded);
    }
    Ok(DecodedBody {
        bytes: current,
        truncated,
    })
}

/// Parse a `Content-Encoding` value into the layers to undo, dropping
/// whitespace and `identity` tokens.
///
/// `identity` is dropped rather than handled as a layer because it means "no
/// transformation was applied": `gzip, identity` is one layer of gzip, and
/// `identity, identity` is zero layers, not an error. The caller has already
/// trimmed and lower-cased the value.
fn split_content_encoding(encoding: &str) -> Vec<&str> {
    encoding
        .split(',')
        .map(str::trim)
        .filter(|layer| !layer.is_empty() && *layer != "identity")
        .collect()
}

/// Undo a single content-coding, returning the bytes and whether they were
/// salvaged from a truncated stream.
fn decode_one_layer(
    body: &[u8],
    layer: &str,
    header: &str,
) -> Result<(Vec<u8>, bool), DecodeError> {
    match layer {
        // `MultiGzDecoder`, not `GzDecoder`: RFC 1952 §2.2 defines a gzip
        // stream as a *series* of members, and `GzDecoder` stops at the first
        // one's trailer. A streaming compressor that flushed mid-body emits
        // several members, and the single-member reader would return that
        // prefix as a clean, untruncated success — a silently short capture
        // that only surfaces later as a parse failure with nothing pointing
        // here. Go's `compress/gzip` reads every member unless asked not to,
        // so this is also what the two paths agreeing requires.
        "gzip" | "x-gzip" => read_capped(flate2::read::MultiGzDecoder::new(body), "gzip", header),
        // zstd frames may be concatenated the same way gzip members are, but
        // there is no multi-frame variant to opt into here: the streaming
        // decoder already spans frames, matching Go. The asymmetry with the
        // gzip arm above is in the two bindings' defaults, not in the policy —
        // `zstd-concatenated-frames` pins that they behave alike.
        "zstd" => {
            let mut decoder =
                zstd::stream::read::Decoder::new(body).map_err(|source| DecodeError::Read {
                    header: header.to_owned(),
                    layer: "zstd",
                    source,
                })?;
            decoder
                .window_log_max(ZSTD_WINDOW_LOG_MAX)
                .map_err(|source| DecodeError::Read {
                    header: header.to_owned(),
                    layer: "zstd",
                    source,
                })?;
            read_capped(decoder, "zstd", header)
        }
        other => Err(DecodeError::Unsupported {
            header: header.to_owned(),
            layer: other.to_owned(),
        }),
    }
}

/// Drain `reader` under the size cap, applying the salvage rule.
///
/// The cap is checked before the error is, so an oversize stream that also ended
/// early is refused rather than salvaged — otherwise the bomb guard would be
/// bypassable by truncating the bomb.
fn read_capped<R: Read>(
    reader: R,
    layer: &'static str,
    header: &str,
) -> Result<(Vec<u8>, bool), DecodeError> {
    // Read one byte past the cap so that a body of exactly MAX_DECODED_BYTES is
    // accepted and one byte more is unambiguously over, without a second read to
    // disambiguate.
    let mut decoded = Vec::new();
    let limit = u64::try_from(MAX_DECODED_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    // On error `read_to_end` leaves everything it did read in the buffer, which
    // is what makes the salvage below possible.
    let result = reader.take(limit).read_to_end(&mut decoded);

    if decoded.len() > MAX_DECODED_BYTES {
        return Err(DecodeError::TooLarge {
            header: header.to_owned(),
            layer,
        });
    }
    match result {
        Ok(_) => Ok((decoded, false)),
        // Salvage on exactly two conjuncts: the stream ended early, and it
        // produced something first. A corrupt header, a bad checksum, or an
        // early end that yielded nothing are all hard failures — there is no
        // partial turn in them to keep.
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof && !decoded.is_empty() => {
            Ok((decoded, true))
        }
        Err(source) => Err(DecodeError::Read {
            header: header.to_owned(),
            layer,
            source,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::io::Write;

    use super::*;

    const BODY: &[u8] = br#"{"model":"gpt-5.1-codex","input":[{"role":"user"}]}"#;

    fn gzipped(body: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        encoder.finish().unwrap()
    }

    fn zstded(body: &[u8]) -> Vec<u8> {
        zstd::encode_all(body, 3).unwrap()
    }

    fn decoded(body: &[u8], encoding: Option<&str>) -> DecodedBody<'static> {
        let got = decode_content_encoding(body, encoding).unwrap();
        DecodedBody {
            bytes: Cow::Owned(got.bytes.into_owned()),
            truncated: got.truncated,
        }
    }

    #[test]
    fn an_absent_or_identity_encoding_passes_the_bytes_through_untouched() {
        for encoding in [
            None,
            Some(""),
            Some("  "),
            Some("identity"),
            Some("IDENTITY"),
        ] {
            assert_eq!(decoded(BODY, encoding).bytes.as_ref(), BODY, "{encoding:?}");
        }
    }

    #[test]
    fn a_zstd_body_decodes() {
        // The encoding pi's Codex provider actually sends, and the one whose
        // absence dropped every turn of a session.
        assert_eq!(decoded(&zstded(BODY), Some("zstd")).bytes.as_ref(), BODY);
    }

    #[test]
    fn a_gzip_body_decodes_under_either_spelling() {
        assert_eq!(decoded(&gzipped(BODY), Some("gzip")).bytes.as_ref(), BODY);
        assert_eq!(decoded(&gzipped(BODY), Some("x-gzip")).bytes.as_ref(), BODY);
    }

    #[test]
    fn every_member_of_a_concatenated_gzip_body_is_read() {
        // RFC 1952 §2.2: a gzip stream is a *series* of members, and a
        // compressor that flushed mid-body emits more than one. `GzDecoder`
        // stops at the first member's trailer and reports that prefix as a
        // clean decode, so this asserts the whole plaintext AND that nothing
        // was flagged as truncated — a decoder that lost the tail here would
        // do it silently, which is the only reason it went unnoticed.
        let mut two_members = gzipped(b"{\"part\":\"one\"}");
        two_members.extend_from_slice(&gzipped(b"{\"part\":\"two\"}"));

        let got = decode_content_encoding(&two_members, Some("gzip")).unwrap();
        assert_eq!(got.bytes.as_ref(), br#"{"part":"one"}{"part":"two"}"#);
        assert!(
            !got.truncated,
            "a complete multi-member stream is not a salvage",
        );
    }

    #[test]
    fn every_frame_of_a_concatenated_zstd_body_is_read() {
        // The same rule for the other coding. It already held — libzstd's
        // streaming decoder spans frames with nothing to opt into — so this
        // pins agreement rather than repairing a divergence, and goes red if a
        // binding change ever drops it.
        let mut two_frames = zstded(b"{\"part\":\"one\"}");
        two_frames.extend_from_slice(&zstded(b"{\"part\":\"two\"}"));

        let got = decode_content_encoding(&two_frames, Some("zstd")).unwrap();
        assert_eq!(got.bytes.as_ref(), br#"{"part":"one"}{"part":"two"}"#);
        assert!(
            !got.truncated,
            "a complete multi-frame stream is not a salvage"
        );
    }

    #[test]
    fn the_encoding_token_is_matched_case_insensitively_and_trimmed() {
        assert_eq!(decoded(&zstded(BODY), Some("  ZStd ")).bytes.as_ref(), BODY);
    }

    #[test]
    fn stacked_layers_are_peeled_outermost_first() {
        // `gzip, zstd` means gzip was applied first and zstd last, so zstd is
        // the outermost layer and comes off first. Peeling left-to-right would
        // hand gzip's decoder a zstd frame.
        let stacked = zstded(&gzipped(BODY));
        assert_eq!(decoded(&stacked, Some("gzip, zstd")).bytes.as_ref(), BODY);
    }

    #[test]
    fn identity_among_real_layers_is_not_a_layer() {
        assert_eq!(
            decoded(&gzipped(BODY), Some("identity, gzip"))
                .bytes
                .as_ref(),
            BODY,
        );
        assert_eq!(
            decoded(BODY, Some("identity, identity")).bytes.as_ref(),
            BODY
        );
    }

    #[test]
    fn an_unsupported_encoding_is_named_rather_than_passed_through() {
        // Passing the bytes through would hand a parser compressed input and
        // report the failure as "not JSON", which is the confusion this exists
        // to prevent.
        let err = decode_content_encoding(BODY, Some("br")).unwrap_err();
        assert!(matches!(err, DecodeError::Unsupported { .. }), "{err:?}");
        assert!(err.to_string().contains("br"), "{err}");
    }

    #[test]
    fn the_failing_layer_and_the_whole_header_are_both_reported() {
        let err = decode_content_encoding(BODY, Some("gzip, deflate")).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("deflate"), "the failing layer: {message}");
        assert!(
            message.contains("gzip, deflate"),
            "the header it came from: {message}",
        );
    }

    #[test]
    fn a_corrupt_stream_that_yielded_nothing_is_refused() {
        let err = decode_content_encoding(b"not a gzip frame at all", Some("gzip")).unwrap_err();
        assert!(matches!(err, DecodeError::Read { .. }), "{err:?}");
    }

    #[test]
    fn a_truncated_gzip_stream_is_salvaged_when_it_produced_output() {
        // A body long enough that the deflate stream emits bytes before the
        // truncation point; the tail (and the gzip trailer) never arrive.
        let long = BODY.repeat(400);
        let full = gzipped(&long);
        let cut = &full[..full.len() * 3 / 4];

        let got = decode_content_encoding(cut, Some("gzip")).unwrap();
        assert!(got.truncated, "the salvage must be reported, not hidden");
        assert!(
            !got.bytes.is_empty() && long.starts_with(got.bytes.as_ref()),
            "a salvaged body must be a prefix of the original",
        );
    }

    #[test]
    fn a_truncated_zstd_stream_is_salvaged_when_it_produced_output() {
        // Big enough to span several zstd blocks: a truncated single-block
        // frame yields nothing at all, which is a hard failure by the rule
        // above rather than a salvage.
        let long = BODY.repeat(8_000);
        let full = zstded(&long);
        let cut = &full[..full.len() * 3 / 4];

        let got = decode_content_encoding(cut, Some("zstd")).unwrap();
        assert!(got.truncated, "the salvage must be reported, not hidden");
        assert!(
            !got.bytes.is_empty() && long.starts_with(got.bytes.as_ref()),
            "a salvaged body must be a prefix of the original",
        );
    }

    #[test]
    fn an_oversize_body_is_refused_rather_than_expanded() {
        let bomb = zstded(&vec![b'a'; MAX_DECODED_BYTES + 1]);
        let err = decode_content_encoding(&bomb, Some("zstd")).unwrap_err();
        assert!(matches!(err, DecodeError::TooLarge { .. }), "{err:?}");
    }

    #[test]
    fn a_body_of_exactly_the_cap_is_accepted() {
        // The cap is a limit, not a strict inequality — the read is one byte
        // long so the two cases are distinguishable without a second read.
        let at_cap = zstded(&vec![b'a'; MAX_DECODED_BYTES]);
        let got = decode_content_encoding(&at_cap, Some("zstd")).unwrap();
        assert_eq!(got.bytes.len(), MAX_DECODED_BYTES);
    }
}
