//! The download engine.
//!
//! Knows nothing about sockets or about Slint. Transports arrive through
//! [`ByteSource`] and the filesystem through [`Storage`], so the engine runs in
//! tests with no network and an injectable failing disk.

pub mod budget;
pub mod cancel;
pub mod chunks;
pub mod download;
pub mod engine;
pub mod error;
pub mod integrity;
pub mod lane;
pub mod model;
pub mod refresh;
pub mod regions;
pub mod resume;
pub mod schedule;
pub mod source;
pub mod store;
pub mod torrent;

pub use budget::{Budget, BudgetChain, Clock, SystemClock};
pub use cancel::Cancel;
pub use chunks::{ChunkProgress, ChunkReport};
pub use download::{DownloadOptions, Outcome, download};
pub use engine::{
    DownloadId, DownloadSnapshot, DownloadSpec, Engine, EngineConfig, SourceFactory, State,
};
pub use error::{Error, Result};
pub use integrity::{Algorithm, Digest, Hasher};
pub use lane::{LaneReport, LaneSelector, LaneSet, SingleLane};
pub use model::{ByteRange, Progress, SourceInfo};
pub use refresh::{
    ApiRefresher, Attempt, CommandRefresher, HttpJson, LinkRefresher, RefreshCoordinator,
    RefreshCtx, RefreshPolicy, RefreshReason, RefreshingSource, ResolvedFetcher, ResolvedSource,
    ResponseSummary, SourceHandle, SourceKey, Staleness, StaticRefresher,
};
pub use resume::{
    ResumeOptions, ResumeOutcome, choose_chunk_size, download_over_lanes, download_resumable,
};
pub use schedule::{LocalTime, Schedule, TimeWindow, Weekday};
pub use source::{ByteSource, ByteStream, Fetch};
pub use store::{FileStorage, ResumableFile, Storage};
pub use torrent::{
    TorrentBackend, TorrentFile, TorrentOutcome, TorrentProgress, TorrentRequest, TorrentSource,
    TorrentStatus, TransferKind, classify, magnet_display_name,
};
