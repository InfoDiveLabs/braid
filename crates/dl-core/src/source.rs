//! The transport boundary.
//!
//! `dl-core` never speaks HTTP; it asks a [`ByteSource`] for metadata and byte
//! streams. Keeps the engine testable without sockets, and lets BitTorrent and
//! segmented media join later as peers of HTTP rather than special cases.

use crate::error::Result;
use crate::model::{ByteRange, SourceInfo};
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;

/// A stream of body bytes. Boxed so sources stay object-safe.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

/// One body request.
#[derive(Clone, Debug, Default)]
pub struct Fetch {
    /// `None` fetches the whole resource.
    pub range: Option<ByteRange>,
    /// A validator the origin must still match, sent as `If-Range`.
    ///
    /// With many chunks in flight over minutes, the resource can change
    /// underneath the download. Carrying the validator on every chunk turns
    /// that into a detected error instead of a file spliced from two versions.
    pub if_range: Option<String>,
}

impl Fetch {
    pub fn whole() -> Self {
        Self::default()
    }

    pub fn range(range: ByteRange) -> Self {
        Self { range: Some(range), if_range: None }
    }

    pub fn validated(range: ByteRange, validator: Option<String>) -> Self {
        Self { range: Some(range), if_range: validator }
    }
}

#[async_trait::async_trait]
pub trait ByteSource: Send + Sync {
    /// Ask the origin what it has, without downloading the body.
    async fn probe(&self) -> Result<SourceInfo>;

    /// Open a body stream.
    ///
    /// Implementations must request `identity` encoding and reject anything
    /// else: a content coding makes the requested range address compressed
    /// bytes while the caller writes decompressed ones. A ranged request
    /// answered with `200` must be reported as [`crate::Error::ResourceChanged`]
    /// rather than treated as the requested range.
    async fn open(&self, fetch: Fetch) -> Result<ByteStream>;
}
