//! Which stretch of the file each connection streams.
//!
//! Every lane used to pull from one shared queue, which threw away everything
//! a connection knew about where it was: each request landed somewhere
//! unrelated to the last, and the origin saw a crowd of clients scattered over
//! the file rather than a few readers walking it.
//!
//! So each connection gets a contiguous run instead, and walks it in order.
//! When one runs out, it takes the back half of whichever run has the most
//! left. That is the whole algorithm, and it does the work of a weighting
//! scheme without being one: a fast connection runs out often and keeps taking
//! halves, a slow one is repeatedly halved and ends up holding roughly what it
//! can finish. The split converges on real speeds rather than on anything we
//! measured and guessed with. Four connections on a hundred-chunk file settle
//! near 0, 25, 50 and 75 without being placed there.
//!
//! It is also why a connection, or a whole lane, can appear mid-transfer. One
//! that turns up thirty seconds in takes the back half of the largest run,
//! exactly like one that merely finished early. There is no separate path.
//!
//! A run matters for more than tidiness: it is what a connection asks the
//! origin for in **one** request, streaming the whole stretch and cutting
//! chunks out of it as the bytes pass. A request per chunk leaves a connection
//! idle for a round trip between each one, which on a real link costs more than
//! half the available bandwidth.
//!
//! Plain round robin was the other candidate. It gives every lane an equal
//! share of chunks regardless of speed, so a phone on mobile data is handed
//! every fourth chunk of a six gigabyte file and the download ends when the
//! phone does. Stealing is what stops that.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// A half-open run of positions in the pending list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// A stretch of the file for one connection to stream in a single request.
///
/// The chunk indices are consecutive, which is what lets the whole run be one
/// byte range. A resumed transfer's holes break a claim into several of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub first: u64,
    /// One past the last chunk.
    pub end: u64,
}

impl Run {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.first)
    }

    pub fn is_empty(&self) -> bool {
        self.first >= self.end
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// Chunk indices still to fetch, ascending.
    ///
    /// A resumed transfer divides what is left rather than the whole file, so
    /// positions are used throughout and chunk numbers only at the edges.
    pending: Vec<u64>,
    /// Runs nobody is working on: the start, and whatever a retired holder
    /// left.
    free: Vec<Span>,
    /// What each connection is walking through, by holder id.
    claims: BTreeMap<usize, Span>,
    /// Which lane each holder is on, so a run of one chunk goes to whichever
    /// would finish it sooner.
    lanes: BTreeMap<usize, (usize, f64)>,
    /// Chunks handed back mid-flight, retried before anything new is started.
    ///
    /// A rate limit or a dead lane returns its chunks here rather than to a
    /// run, because they no longer belong to anyone's stretch of the file and
    /// burying them inside one would leave them until that holder got there.
    orphans: Vec<u64>,
}

impl Inner {
    /// The best rate among the holders on a lane, or zero if unmeasured.
    fn rate_of_holder(&self, holder: usize) -> f64 {
        self.lanes.get(&holder).map(|(_, rate)| *rate).unwrap_or(0.0)
    }

    /// Find a run for a connection that has none.
    ///
    /// `rate` is what the asking lane is currently managing, and matters only
    /// for a run of a single chunk: that cannot be halved, so taking it means
    /// taking it away, and handing the file's last chunk from a gigabit card to
    /// a phone is how a download that was seconds from finishing spends ten
    /// more on four megabytes.
    fn acquire(&mut self, holder: usize, rate: f64) -> Option<Span> {
        self.free.retain(|span| !span.is_empty());
        // The longest, and the earliest of those: ties are broken towards the
        // front of the file so runs are handed out in order and the file fills
        // roughly front to back.
        let longest =
            (0..self.free.len()).max_by_key(|i| (self.free[*i].len(), std::cmp::Reverse(*i)));
        if let Some(index) = longest {
            return Some(self.free.remove(index));
        }

        // Nothing spare, so take the back half of whoever has the most left.
        // The front half stays with its owner, which is the part it is already
        // walking towards.
        let victim = self
            .claims
            .iter()
            .filter(|(owner, _)| **owner != holder)
            .max_by_key(|(owner, span)| (span.len(), std::cmp::Reverse(**owner)))
            .map(|(owner, _)| *owner)?;
        let held = self.rate_of_holder(victim);
        let span = self.claims.get_mut(&victim)?;
        if span.is_empty() {
            return None;
        }
        if span.len() == 1 && rate <= held {
            // Nothing to gain: the connection that has it will not be slower.
            return None;
        }
        // Halving a run of one leaves the owner with nothing and hands the
        // chunk over whole, which is what is wanted when the asker is faster.
        let middle = span.start + span.len() / 2;
        let back = Span { start: middle, end: span.end };
        span.end = middle;
        Some(back)
    }

    /// The consecutive chunks at the head of a span, at most `limit` of them.
    fn run_at(&self, span: Span, limit: usize) -> Option<Run> {
        let first = *self.pending.get(span.start)?;
        let mut end = first + 1;
        let mut at = span.start + 1;
        while at < span.end && (at - span.start) < limit.max(1) {
            match self.pending.get(at) {
                // A hole in a resumed file ends the run: one request cannot
                // skip the chunks already on disk, and asking for them again
                // would re-download what the journal already has.
                Some(next) if *next == end => end += 1,
                _ => break,
            }
            at += 1;
        }
        Some(Run { first, end })
    }
}

/// The file divided into one run per connection.
#[derive(Debug)]
pub struct Regions {
    inner: Mutex<Inner>,
}

impl Regions {
    /// `pending` is the chunks still to fetch, in ascending order, and `ways`
    /// how many stretches to divide them into up front.
    ///
    /// Dividing at the start rather than letting the first connection take the
    /// lot and the others steal it back: both end up balanced, but only this
    /// one starts balanced, and a transfer that is over in four chunks never
    /// gets the chance to rebalance.
    pub fn new(pending: Vec<u64>, ways: usize) -> Self {
        let mut free = Vec::new();
        if !pending.is_empty() {
            let ways = ways.clamp(1, pending.len());
            let each = pending.len() / ways;
            let spare = pending.len() % ways;
            let mut start = 0;
            for lane in 0..ways {
                // The remainder goes to the earliest runs rather than all to
                // the last one, which would leave one holder with a run the
                // length of the shortfall.
                let end = start + each + usize::from(lane < spare);
                free.push(Span { start, end });
                start = end;
            }
        }
        Self { inner: Mutex::new(Inner { pending, free, ..Default::default() }) }
    }

    /// The next stretch for this connection to stream, of at most `limit`
    /// chunks, or `None` when there is nothing left worth handing it.
    ///
    /// `lane` and `rate` describe the path it is on: the rate decides only who
    /// gets a run that is down to its last chunk.
    pub fn begin(&self, holder: usize, lane: usize, rate: f64, limit: usize) -> Option<Run> {
        let mut inner = self.inner.lock().unwrap();
        inner.lanes.insert(holder, (lane, rate));

        // Before anything else: chunks somebody handed back are already
        // overdue. Consecutive ones are streamed together like any other run.
        if !inner.orphans.is_empty() {
            inner.orphans.sort_unstable();
            let first = inner.orphans[0];
            let mut taken = 1;
            while taken < limit.max(1)
                && inner.orphans.get(taken).is_some_and(|next| *next == first + taken as u64)
            {
                taken += 1;
            }
            inner.orphans.drain(..taken);
            return Some(Run { first, end: first + taken as u64 });
        }

        let held = inner.claims.get(&holder).copied().unwrap_or(Span { start: 0, end: 0 });
        let span = if held.is_empty() {
            let fresh = inner.acquire(holder, rate)?;
            inner.claims.insert(holder, fresh);
            fresh
        } else {
            held
        };
        let run = inner.run_at(span, limit)?;
        // The head of the run is in flight from here, so the claim no longer
        // offers it. Leaving it there was a real bug: another connection could
        // be handed the chunk this one had just started streaming.
        if let Some(claim) = inner.claims.get_mut(&holder) {
            claim.start += 1;
        }
        Some(run)
    }

    /// Take the next chunk of this connection's claim, so the stream may go on
    /// to it.
    ///
    /// `false` means there is nothing left to take: a faster lane took the
    /// back of the claim while this one was streaming it, and carrying on
    /// would fetch what somebody else is now fetching. Nothing is owed in that
    /// case, because the chunks were never this connection's to hand back.
    pub fn take_next(&self, holder: usize) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(span) = inner.claims.get_mut(&holder) else { return false };
        if span.is_empty() {
            return false;
        }
        span.start += 1;
        true
    }

    /// Hand chunks back without having fetched them.
    pub fn give_back(&self, chunks: impl IntoIterator<Item = u64>) {
        self.inner.lock().unwrap().orphans.extend(chunks);
    }

    /// This connection is finished. Whatever it had left goes back on the pile.
    pub fn retire(&self, holder: usize) {
        let mut inner = self.inner.lock().unwrap();
        inner.lanes.remove(&holder);
        if let Some(span) = inner.claims.remove(&holder)
            && !span.is_empty()
        {
            inner.free.push(span);
        }
    }

    /// Chunks still to be handed out. Anything already streaming is not
    /// counted: it has been taken, and only a hand-back brings it back.
    pub fn remaining(&self) -> usize {
        let inner = self.inner.lock().unwrap();
        inner.orphans.len()
            + inner.free.iter().map(Span::len).sum::<usize>()
            + inner.claims.values().map(Span::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOTS: usize = 1024;

    fn regions(count: u64, ways: usize) -> Regions {
        Regions::new((0..count).collect(), ways)
    }

    /// Stream a whole run, as a connection does: take it, then report each
    /// chunk until the run ends or somebody takes the rest.
    fn walk(r: &Regions, holder: usize, rate: f64, limit: usize) -> Vec<u64> {
        let mut seen = Vec::new();
        while let Some(run) = r.begin(holder, holder, rate, limit) {
            seen.push(run.first);
            for chunk in run.first + 1..run.end {
                if !r.take_next(holder) {
                    break;
                }
                seen.push(chunk);
            }
        }
        seen
    }

    #[test]
    fn one_connection_walks_the_file_in_order() {
        let r = regions(8, 1);
        assert_eq!(walk(&r, 0, 0.0, LOTS), (0..8).collect::<Vec<_>>());
        assert!(r.is_empty());
    }

    #[test]
    fn a_whole_stretch_comes_back_as_one_request() {
        // The point of a run: one request for the lot, not one per chunk.
        let r = regions(64, 1);
        let run = r.begin(0, 0, 0.0, LOTS).unwrap();
        assert_eq!(run, Run { first: 0, end: 64 });
    }

    #[test]
    fn a_run_is_capped_so_the_tail_is_not_one_long_request() {
        let r = regions(64, 1);
        assert_eq!(r.begin(0, 0, 0.0, 8).unwrap(), Run { first: 0, end: 8 });
    }

    #[test]
    fn connections_settle_across_the_file_rather_than_on_top_of_each_other() {
        // Four connections on a hundred chunks end up near 0, 25, 50 and 75
        // without being placed there: each new one halves the largest run.
        let r = regions(100, 1);
        let mut starts = Vec::new();
        for holder in 0..4 {
            starts.push(r.begin(holder, holder, 0.0, LOTS).unwrap().first);
        }
        starts.sort_unstable();
        assert_eq!(starts, vec![0, 25, 50, 75]);
    }

    #[test]
    fn two_lanes_are_interleaved_rather_than_split_down_the_middle() {
        // Two connections each: one lane holds the first and third stretch,
        // the other the second and fourth, so losing a lane costs pieces
        // spread through the file rather than half of it in one block.
        let r = regions(100, 2);
        let a1 = r.begin(0, 0, 0.0, LOTS).unwrap().first;
        let b1 = r.begin(1, 1, 0.0, LOTS).unwrap().first;
        let a2 = r.begin(2, 0, 0.0, LOTS).unwrap().first;
        let b2 = r.begin(3, 1, 0.0, LOTS).unwrap().first;
        assert_eq!((a1, b1), (0, 50));
        assert_eq!((a2, b2), (25, 75));
    }

    #[test]
    fn a_connection_that_joins_late_is_given_a_stretch_of_its_own() {
        // A phone paired thirty seconds in has to start carrying bytes without
        // the transfer being restarted, and it takes the same path as a
        // connection that merely ran out early.
        let r = regions(16, 1);
        let run = r.begin(0, 0, 0.0, 4).unwrap();
        for _ in run.first + 1..run.end {
            r.take_next(0);
        }
        let joined = r.begin(1, 1, 0.0, LOTS).expect("the new connection gets work");
        assert!(joined.first >= 8, "it started at {}, inside work under way", joined.first);
        assert!(joined.first < 16);
    }

    #[test]
    fn a_stream_stops_where_a_faster_lane_took_over() {
        // The run was handed out whole, but the back of it can be taken while
        // it is being streamed. Carrying on would fetch what somebody else is
        // now fetching.
        let r = regions(16, 1);
        let run = r.begin(0, 0, 1.0, LOTS).unwrap();
        assert_eq!(run.end, 16);

        r.begin(1, 1, 1.0, LOTS).expect("the second connection takes the back half");
        let mut reached = 1;
        while r.take_next(0) {
            reached += 1;
            assert!(reached < 16, "the stream never noticed it had been cut short");
        }
        assert!(reached < 16, "stopped at {reached}");
    }

    #[test]
    fn a_fast_lane_takes_over_from_a_slow_one_rather_than_waiting() {
        // Round robin would hand a slow lane a fixed share and let it decide
        // when the download finishes. Here the fast one keeps taking halves
        // and the slow one is left with what it can manage.
        let r = regions(64, 2);
        let slow = r.begin(1, 1, 400e3, LOTS).unwrap().first;
        let fast = walk(&r, 0, 15e6, LOTS);
        assert!(fast.len() > 50, "the idle connection kept {} chunks", 64 - fast.len());
        assert!(!fast.contains(&slow), "two connections took the same chunk");
    }

    #[test]
    fn a_run_of_one_goes_to_whichever_lane_would_finish_it_sooner() {
        let r = regions(4, 2);
        assert_eq!(r.begin(0, 0, 400e3, 1).unwrap().first, 0);
        assert_eq!(r.begin(1, 1, 400e3, 1).unwrap().first, 2);
        // Holder 0 still has chunk 1 to give and is slow; holder 1 comes back
        // fast and takes it over.
        assert_eq!(r.begin(1, 1, 15e6, 1).unwrap().first, 3);
        assert_eq!(r.begin(1, 1, 15e6, 1).unwrap().first, 1, "the faster lane should take it");
    }

    #[test]
    fn a_slower_lane_does_not_take_work_away_from_a_faster_one() {
        let r = regions(4, 2);
        assert_eq!(r.begin(0, 0, 15e6, 1).unwrap().first, 0);
        assert_eq!(r.begin(1, 1, 400e3, 1).unwrap().first, 2);
        assert_eq!(r.begin(1, 1, 400e3, 1).unwrap().first, 3);
        assert!(r.begin(1, 1, 400e3, 1).is_none(), "the slow lane took the fast lane's chunk");
    }

    #[test]
    fn nothing_is_handed_out_twice() {
        let r = regions(50, 4);
        let mut seen = Vec::new();
        for round in 0..80 {
            for holder in 0..4 {
                if round % (holder + 1) == 0
                    && let Some(run) = r.begin(holder, holder, 0.0, 3)
                {
                    seen.push(run.first);
                    for chunk in run.first + 1..run.end {
                        if !r.take_next(holder) {
                            break;
                        }
                        seen.push(chunk);
                    }
                }
            }
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "a chunk was given out twice");
        assert_eq!(sorted, (0..50).collect::<Vec<_>>(), "some chunk was never handed out");
        assert!(r.is_empty());
    }

    #[test]
    fn chunks_handed_back_are_retried_before_anything_new() {
        // They are already overdue: burying them in some run would leave them
        // until that connection walked to them, which may be the end.
        let r = regions(32, 1);
        r.begin(0, 0, 0.0, LOTS);
        r.give_back([5, 6, 7]);
        // And consecutive ones come back as one run, not three requests.
        assert_eq!(r.begin(1, 1, 0.0, LOTS).unwrap(), Run { first: 5, end: 8 });
    }

    #[test]
    fn a_retired_connections_stretch_goes_back_on_the_pile() {
        // An interface that drops must not take a quarter of the file with it.
        let r = regions(16, 2);
        r.begin(0, 0, 0.0, LOTS);
        r.begin(1, 1, 0.0, LOTS);
        let left = r.remaining();
        r.retire(1);
        assert_eq!(r.remaining(), left, "retiring lost work");
        assert_eq!(walk(&r, 0, 0.0, LOTS).len(), left, "the survivor could not pick it up");
    }

    #[test]
    fn a_resumed_transfer_never_asks_for_a_chunk_it_already_has() {
        // Positions, not chunk numbers: the journal's holes are not a range,
        // and one request cannot skip over them.
        let r = Regions::new(vec![3, 4, 9, 20, 21], 1);
        assert_eq!(r.begin(0, 0, 0.0, LOTS).unwrap(), Run { first: 3, end: 5 });
        assert!(r.take_next(0));
        assert_eq!(r.begin(0, 0, 0.0, LOTS).unwrap(), Run { first: 9, end: 10 });
    }

    #[test]
    fn an_empty_file_hands_out_nothing() {
        let r = Regions::new(Vec::new(), 4);
        assert!(r.is_empty());
        assert!(r.begin(0, 0, 0.0, LOTS).is_none());
    }
}
