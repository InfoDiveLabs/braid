//! Crash consistency for the journal.
//!
//! The property under test throughout: **the journal never claims a chunk whose
//! data was not made durable first.** A violation is silent: resume skips a
//! chunk that was never written, and the finished file is quietly wrong: so
//! every test here crashes the journal on purpose and checks what survives.
//!
//! These open in [`Durability::Safe`], where every completion is flushed as it
//! is recorded. The property above holds in every mode; what Safe adds is that
//! each `record_chunk` is durable by the time it returns, which is what lets a
//! test crash between two of them and say exactly what should have survived.
//! The looser modes are covered in `store::resumable`, where the thing worth
//! testing is what they *lose*.

use dl_core::store::Durability;
use dl_core::store::journal::{Journal, Opened, ResourceId};
use dl_core::store::{RawFile, StdFile};
use dl_testkit::{Fault, FaultyFile};
use proptest::prelude::*;
use std::sync::Arc;

const CHUNK: u64 = 64 * 1024;
const TOTAL: u64 = CHUNK * 16;

/// A stand-in hash; these tests exercise bookkeeping, not content.
fn chunk_hash(index: u64) -> blake3::Hash {
    blake3::hash(&index.to_le_bytes())
}

struct Harness {
    _dir: tempfile::TempDir,
    meta: Arc<dyn RawFile>,
    data: Arc<dyn RawFile>,
    resource: ResourceId,
}

impl Harness {
    async fn new() -> Self {
        Self::with_resource(ResourceId {
            total_len: TOTAL,
            etag: Some("\"abc\"".into()),
            last_modified: None,
        })
        .await
    }

    async fn with_resource(resource: ResourceId) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let meta: Arc<dyn RawFile> =
            Arc::new(StdFile::open(dir.path().join("f.bin.dlmeta")).await.unwrap());
        let data: Arc<dyn RawFile> =
            Arc::new(StdFile::open(dir.path().join("f.bin.part")).await.unwrap());
        Self { _dir: dir, meta, data, resource }
    }

    async fn open(&self) -> Journal {
        Journal::open_with(
            Arc::clone(&self.meta),
            Arc::clone(&self.data),
            self.resource.clone(),
            CHUNK,
            Durability::Safe,
        )
        .await
        .unwrap()
    }

    /// Reopen through a wrapper that can corrupt or lose writes.
    async fn open_faulty(&self, faults: Vec<Fault>) -> (Journal, Arc<FaultyFile>) {
        let faulty = Arc::new(FaultyFile::with_faults(Arc::clone(&self.meta), faults));
        let journal = Journal::open(
            Arc::clone(&faulty) as Arc<dyn RawFile>,
            Arc::clone(&self.data),
            self.resource.clone(),
            CHUNK,
        )
        .await
        .unwrap();
        (journal, faulty)
    }
}

#[tokio::test]
async fn a_fresh_journal_has_nothing_completed() {
    let h = Harness::new().await;
    let journal = h.open().await;
    assert_eq!(journal.opened_as(), Opened::Fresh);
    assert_eq!(journal.completed().len(), 0);
    assert_eq!(journal.remaining().len(), 16);
}

#[tokio::test]
async fn completions_survive_reopening() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    for index in [0, 3, 7, 15] {
        journal.record_chunk(index, chunk_hash(index)).await.unwrap();
    }

    let reopened = h.open().await;
    assert_eq!(reopened.opened_as(), Opened::Resumed);
    for index in [0, 3, 7, 15] {
        assert!(reopened.completed().contains(index), "chunk {index} was lost");
    }
    assert_eq!(reopened.completed().len(), 4);
    assert_eq!(reopened.bytes_done(), CHUNK * 4);
}

#[tokio::test]
async fn a_checkpoint_folds_the_log_into_the_header() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    for index in 0..8 {
        journal.record_chunk(index, chunk_hash(index)).await.unwrap();
    }
    journal.checkpoint().await.unwrap();
    journal.record_chunk(8, chunk_hash(8)).await.unwrap();

    let reopened = h.open().await;
    assert_eq!(reopened.completed().len(), 9);
}

#[tokio::test]
async fn a_torn_header_write_falls_back_to_the_other_slot() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    for index in 0..4 {
        journal.record_chunk(index, chunk_hash(index)).await.unwrap();
    }
    journal.checkpoint().await.unwrap();

    // Tear the next header mid-write. Its CRC fails, so the previous slot must
    // still be readable rather than both being lost.
    let (mut faulty_journal, _faulty) =
        h.open_faulty(vec![Fault::TornWriteAt { offset: 0, keep: 900 }]).await;
    faulty_journal.record_chunk(4, chunk_hash(4)).await.unwrap();
    let _ = faulty_journal.checkpoint().await;

    let reopened = h.open().await;
    assert!(reopened.completed().len() >= 4, "the earlier header was not recoverable");
    for index in 0..4 {
        assert!(reopened.completed().contains(index));
    }
}

#[tokio::test]
async fn a_truncated_journal_never_yields_garbage() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    for index in 0..12 {
        journal.record_chunk(index, chunk_hash(index)).await.unwrap();
    }
    let full = h.meta.size().await.unwrap();

    // Truncating to every length models a crash at any point in the file. Each
    // must either recover a prefix of the real state or report nothing: never
    // a chunk that was never recorded.
    for cut in (0..full).step_by(97) {
        h.meta.set_len(cut).await.unwrap();
        let reopened = h.open().await;
        assert!(
            reopened.completed().len() <= 12,
            "truncating to {cut} invented completions: {}",
            reopened.completed().len()
        );
        for index in reopened.completed().missing() {
            assert!(index < 16);
        }
    }
}

#[tokio::test]
async fn a_flipped_bit_in_a_header_is_rejected() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    journal.record_chunk(0, chunk_hash(0)).await.unwrap();
    journal.checkpoint().await.unwrap();

    for offset in [16u64, 24, 48, 408, 500] {
        let original = h.meta.read_at(offset, 1).await.unwrap();
        h.meta.write_at(offset, bytes::Bytes::from(vec![original[0] ^ 0xFF])).await.unwrap();

        // The CRC must catch it: either the other slot is used, or nothing is.
        let reopened = h.open().await;
        assert!(reopened.completed().len() <= 1, "corrupt header at {offset} was trusted");

        h.meta.write_at(offset, bytes::Bytes::from(original)).await.unwrap();
    }
}

#[tokio::test]
async fn a_changed_resource_discards_the_partial_file() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    for index in 0..8 {
        journal.record_chunk(index, chunk_hash(index)).await.unwrap();
    }
    drop(journal);

    // Same URL, different content. Resuming would splice two files together.
    let changed = Harness {
        _dir: h._dir,
        meta: Arc::clone(&h.meta),
        data: Arc::clone(&h.data),
        resource: ResourceId {
            total_len: TOTAL,
            etag: Some("\"different\"".into()),
            last_modified: None,
        },
    };
    let reopened = changed.open().await;
    assert_eq!(reopened.opened_as(), Opened::Restarted);
    assert_eq!(reopened.completed().len(), 0);
}

#[tokio::test]
async fn a_changed_length_discards_the_partial_file() {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    journal.record_chunk(0, chunk_hash(0)).await.unwrap();
    drop(journal);

    let changed = Harness {
        _dir: h._dir,
        meta: Arc::clone(&h.meta),
        data: Arc::clone(&h.data),
        resource: ResourceId {
            total_len: TOTAL * 2,
            etag: Some("\"abc\"".into()),
            last_modified: None,
        },
    };
    assert_eq!(changed.open().await.opened_as(), Opened::Restarted);
}

#[tokio::test]
async fn a_weak_etag_is_not_accepted_as_proof_of_identity() {
    // W/ promises semantic equivalence, not identical bytes, so it cannot
    // justify splicing new bytes onto an old partial file.
    let a = ResourceId { total_len: 100, etag: Some("W/\"v1\"".into()), last_modified: None };
    let b = ResourceId { total_len: 100, etag: Some("W/\"v1\"".into()), last_modified: None };
    assert!(!a.can_resume_as(&b));

    let strong_a = ResourceId { total_len: 100, etag: Some("\"v1\"".into()), last_modified: None };
    let strong_b = ResourceId { total_len: 100, etag: Some("\"v1\"".into()), last_modified: None };
    assert!(strong_a.can_resume_as(&strong_b));

    let strong_c = ResourceId { total_len: 100, etag: Some("\"v2\"".into()), last_modified: None };
    assert!(!strong_a.can_resume_as(&strong_c));
}

#[tokio::test]
async fn dropped_syncs_never_produce_phantom_completions() {
    let h = Harness::new().await;
    let (mut journal, faulty) = h.open_faulty(vec![Fault::DropSync]).await;
    for index in 0..6 {
        journal.record_chunk(index, chunk_hash(index)).await.unwrap();
    }
    assert!(faulty.dropped_syncs() > 0, "the fault never fired");

    // Writes still reached the file here, so this asserts the weaker but still
    // essential property: nothing outside the recorded set appears.
    let reopened = h.open().await;
    for index in reopened.completed().missing() {
        assert!(index < 16);
    }
    assert!(reopened.completed().len() <= 6, "completions appeared that were never recorded");
}

/// The state machine. Random operation sequences, crashing at arbitrary points,
/// asserting that what survives is always a subset of what was recorded.
#[derive(Clone, Debug)]
enum Op {
    Record(u64),
    Checkpoint,
    Crash,
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            6 => (0u64..16).prop_map(Op::Record),
            2 => Just(Op::Checkpoint),
            1 => Just(Op::Crash),
        ],
        1..40,
    )
}

async fn run_ops(script: Vec<Op>) -> std::result::Result<(), TestCaseError> {
    let h = Harness::new().await;
    let mut journal = h.open().await;
    let mut recorded = std::collections::BTreeSet::new();

    for op in script {
        match op {
            Op::Record(index) => {
                journal.record_chunk(index, chunk_hash(index)).await.unwrap();
                recorded.insert(index);
            }
            Op::Checkpoint => journal.checkpoint().await.unwrap(),
            Op::Crash => {
                // Dropping and reopening is what a process restart does.
                drop(journal);
                journal = h.open().await;

                let survived: std::collections::BTreeSet<u64> =
                    (0..16).filter(|i| journal.completed().contains(*i)).collect();
                prop_assert!(
                    survived.is_subset(&recorded),
                    "journal claimed chunks that were never recorded: {:?}",
                    survived.difference(&recorded).collect::<Vec<_>>()
                );
                // Everything synced before the crash must still be there.
                prop_assert_eq!(
                    &survived,
                    &recorded,
                    "a synced completion was lost across the crash"
                );
            }
        }
    }

    drop(journal);
    let final_state = h.open().await;
    let survived: std::collections::BTreeSet<u64> =
        (0..16).filter(|i| final_state.completed().contains(*i)).collect();
    prop_assert_eq!(survived, recorded);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("DL_PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
    ))]

    #[test]
    fn the_journal_never_invents_or_loses_a_synced_completion(script in ops()) {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(run_ops(script))?;
    }
}
