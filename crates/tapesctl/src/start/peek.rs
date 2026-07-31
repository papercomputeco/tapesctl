//! Bounded body peek.
//!
//! The proxy has to *inspect* a request body — it ships the body to ingest as
//! the turn's `request` — while still forwarding it upstream byte-for-byte.
//! Those two goals conflict the moment anyone can hold "the prefix" and "the
//! rest" as separate values, because then duplicating or skipping bytes becomes
//! expressible.
//!
//! [`BoundedPeek::peek`] makes that unexpressible: it consumes `self` and hands
//! back the buffered prefix *and* a [`Replay`] that re-emits that same prefix in
//! front of the remainder. There is no way to obtain the body without the
//! prefix, so the upstream always sees the original byte sequence.
//!
//! # Why bounded
//!
//! The cap is what keeps a hostile or merely enormous body from pinning memory.
//! When a body exceeds it, the excess is *not* buffered — it streams straight
//! through — and [`Peeked::complete`] reports `false`. The proxy uses that to
//! decide it cannot describe this turn to ingest, and it then skips the capture
//! rather than posting a truncated request that the server would reject. That
//! ordering is the invariant worth stating plainly: **capture degrades,
//! forwarding never does.**
//!
//! Ported from paperd's `proxy::peek`, whose contract this preserves; the
//! `complete` flag is the addition, because paperd peeks only to enrich a log
//! line and does not care whether it saw the whole body.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame};
use http_body_util::BodyExt;

use crate::error::{Error, Result};

/// A body wrapper that buffers up to `max_peek` bytes of the head.
pub struct BoundedPeek<B> {
    body: B,
    max_peek: usize,
}

/// The buffered head of a body, plus whether it is the whole body.
#[derive(Debug, Clone)]
pub struct Peeked {
    /// The buffered bytes.
    pub prefix: Bytes,
    /// True when the body ended within the cap, so `prefix` is all of it.
    pub complete: bool,
}

impl Peeked {
    /// The prefix, but only when it is known to be the entire body.
    #[must_use]
    pub fn whole_body(&self) -> Option<&Bytes> {
        self.complete.then_some(&self.prefix)
    }
}

impl<B> BoundedPeek<B>
where
    B: Body<Data = Bytes> + Unpin + Send + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    /// Wrap `body` with a peek window of `max_peek` bytes.
    pub fn new(body: B, max_peek: usize) -> Self {
        Self { body, max_peek }
    }

    /// Buffer the head of the body, returning it alongside a [`Replay`] that
    /// streams the buffered bytes followed by whatever remains.
    pub async fn peek(self) -> Result<(Peeked, Replay<B>)> {
        let Self { mut body, max_peek } = self;

        let mut accumulated = BytesMut::with_capacity(max_peek.min(64 * 1024));
        let mut overflow: Option<Bytes> = None;
        // Assume completeness and clear it on overflow: the loop exits either
        // because the body ended (complete) or because the cap was hit (not).
        let mut complete = true;

        while accumulated.len() < max_peek {
            let Some(frame) = Pin::new(&mut body).frame().await else {
                break; // body EOF inside the cap
            };
            let frame = frame.map_err(|err| Error::RequestBody {
                source: Box::new(err),
            })?;
            // Only data frames are buffered. Trailers are exotic on request
            // bodies and would complicate the prefix invariant; `Replay`
            // delegates them through unchanged once the prefix is drained.
            let Ok(data) = frame.into_data() else {
                continue;
            };
            let need = max_peek - accumulated.len();
            if data.len() <= need {
                accumulated.extend_from_slice(&data);
            } else {
                // A frame straddling the cap: buffer what fits and hold the
                // remainder as the first thing replayed after the prefix.
                accumulated.extend_from_slice(&data[..need]);
                overflow = Some(data.slice(need..));
                complete = false;
            }
        }
        if accumulated.len() >= max_peek && overflow.is_none() {
            // Filled the window exactly. Whether more follows is unknown
            // without another poll, and polling here would buffer past the
            // cap — so treat it as incomplete rather than risk claiming a
            // truncated body is whole.
            complete = false;
        }

        let prefix = accumulated.freeze();
        Ok((
            Peeked {
                prefix: prefix.clone(),
                complete,
            },
            Replay {
                prefix: Some(prefix),
                overflow,
                body,
            },
        ))
    }
}

/// Streams a peeked prefix, then any straddling overflow, then the rest of the
/// original body.
pub struct Replay<B> {
    prefix: Option<Bytes>,
    overflow: Option<Bytes>,
    body: B,
}

impl<B> Body for Replay<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(prefix) = self.prefix.take() {
            // A zero-length prefix would be a pointless empty frame; skip
            // straight to whatever is next.
            if !prefix.is_empty() {
                return Poll::Ready(Some(Ok(Frame::data(prefix))));
            }
        }
        if let Some(overflow) = self.overflow.take() {
            return Poll::Ready(Some(Ok(Frame::data(overflow))));
        }
        Pin::new(&mut self.body).poll_frame(cx)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use http_body_util::{Full, StreamBody};

    async fn drain<B>(body: B) -> Vec<u8>
    where
        B: Body<Data = Bytes> + Unpin,
        B::Error: std::fmt::Debug,
    {
        let collected = body.collect().await.unwrap();
        collected.to_bytes().to_vec()
    }

    fn chunked(chunks: Vec<&'static [u8]>) -> impl Body<Data = Bytes, Error = std::io::Error> {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(Frame::data(Bytes::from_static(c)))),
        );
        StreamBody::new(stream)
    }

    #[tokio::test]
    async fn a_small_body_is_peeked_whole_and_replayed_intact() {
        let body = Full::new(Bytes::from_static(b"{\"model\":\"x\"}"));
        let (peeked, replay) = BoundedPeek::new(body, 1024).peek().await.unwrap();

        assert!(peeked.complete);
        assert_eq!(peeked.whole_body().unwrap().as_ref(), b"{\"model\":\"x\"}");
        assert_eq!(drain(replay).await, b"{\"model\":\"x\"}");
    }

    #[tokio::test]
    async fn an_oversize_body_still_forwards_every_byte() {
        // The whole point: capture may give up, forwarding may not.
        let body = chunked(vec![b"aaaaaaaa", b"bbbbbbbb", b"cccccccc"]);
        let (peeked, replay) = BoundedPeek::new(body, 10).peek().await.unwrap();

        assert!(!peeked.complete, "a truncated peek must report itself");
        assert!(peeked.whole_body().is_none());
        assert_eq!(drain(replay).await, b"aaaaaaaabbbbbbbbcccccccc");
    }

    #[tokio::test]
    async fn a_frame_straddling_the_cap_is_split_without_loss_or_duplication() {
        let body = chunked(vec![b"0123456789abcdef"]);
        let (peeked, replay) = BoundedPeek::new(body, 6).peek().await.unwrap();

        assert_eq!(peeked.prefix.as_ref(), b"012345");
        assert!(!peeked.complete);
        assert_eq!(drain(replay).await, b"0123456789abcdef");
    }

    #[tokio::test]
    async fn a_body_exactly_filling_the_window_is_not_claimed_to_be_complete() {
        // Knowing whether more follows would require polling past the cap.
        // Under-claiming costs one skipped capture; over-claiming would post a
        // truncated request body as if it were whole.
        let body = chunked(vec![b"012345"]);
        let (peeked, replay) = BoundedPeek::new(body, 6).peek().await.unwrap();

        assert_eq!(peeked.prefix.as_ref(), b"012345");
        assert!(!peeked.complete);
        assert_eq!(drain(replay).await, b"012345");
    }

    #[tokio::test]
    async fn an_empty_body_peeks_empty_and_replays_empty() {
        let body = Full::new(Bytes::new());
        let (peeked, replay) = BoundedPeek::new(body, 1024).peek().await.unwrap();

        assert!(peeked.complete);
        assert!(peeked.prefix.is_empty());
        assert!(drain(replay).await.is_empty());
    }

    #[tokio::test]
    async fn a_multi_frame_body_within_the_cap_is_joined_and_replayed_in_order() {
        let body = chunked(vec![b"{\"a\":", b"1}"]);
        let (peeked, replay) = BoundedPeek::new(body, 1024).peek().await.unwrap();

        assert!(peeked.complete);
        assert_eq!(peeked.prefix.as_ref(), b"{\"a\":1}");
        assert_eq!(drain(replay).await, b"{\"a\":1}");
    }
}
