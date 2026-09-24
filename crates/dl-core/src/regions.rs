//! Which part of the file each lane works through.
//!
//! Every lane used to pull from one shared queue, which meant four interfaces
//! all reaching into the same pile and taking whatever was on top. It works,
//! but it throws away everything a lane knows about where it was: each request
//! lands somewhere unrelated to the last, the origin sees four clients
//! scattered across the file rather than four readers walking it, and nothing
//! about a lane's position tells you anything.
//!
//! So each lane gets a contiguous run of the file instead, and walks it in
//! order. When a lane runs out, it takes the back half of whichever run has
//! the most left. That is the whole algorithm, and it does the work of a
//! weighting scheme without needing one: a fast lane runs out often and keeps
//! taking halves, a slow lane is repeatedly halved and ends up holding roughly
//! what it can actually finish. The split converges on the lanes' real speeds
//! rather than on anything we measured and guessed with.
//!
//! It is also why a lane can appear mid-transfer. A phone paired thirty
//! seconds in takes the back half of the largest run, exactly like a lane that
//! merely finished early. There is no separate path for it.
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

#[derive(Debug, Default)]
struct Inner {
    /// Chunk indices still to fetch, ascending.
    ///
    /// A resumed transfer divides what is left rather than the whole file, so
    /// positions are used throughout and chunk numbers only at the edges.
    pending: Vec<u64>,
    /// Runs nobody is working on: the start, and whatever a retired lane left.
    free: Vec<Span>,
    /// What each lane is walking through.
    claims: BTreeMap<usize, Span>,
    /// Chunks handed back mid-flight, retried before anything new is started.
    ///
    /// A rate limit or a dead lane returns its chunk here rather than to a
    /// run, because it no longer belongs to anyone's stretch of the file and
    /// burying it inside one would leave it until that lane got there.
    orphans: Vec<u64>,
}

impl Inner {
    /// Find a run for a lane that has none.
    fn acquire(&mut self) -> Option<Span> {
        self.free.retain(|span| !span.is_empty());
        // The longest, and the earliest of those: ties are broken towards the
        // front of the file so lanes are handed their runs in order and the
        // file fills roughly front to back.
        let longest =
            (0..self.free.len()).max_by_key(|i| (self.free[*i].len(), std::cmp::Reverse(*i)));
        if let Some(index) = longest {
            return Some(self.free.remove(index));
        }

        // Nothing spare, so take the back half of whoever has the most left.
        // The front half stays with its owner, which is the part it is already
        // walking towards.
        let victim = *self.claims.iter().max_by_key(|(_, span)| span.len())?.0;
        let span = self.claims.get_mut(&victim)?;
        if span.len() < 2 {
            // One chunk cannot be halved, and taking it outright would race
            // the lane that is about to fetch it.
            return None;
        }
        let middle = span.start + span.len() / 2;
        let back = Span { start: middle, end: span.end };
        span.end = middle;
        Some(back)
    }
}

/// The file divided into one run per lane.
#[derive(Debug)]
pub struct Regions {
    inner: Mutex<Inner>,
}

impl Regions {
    /// `pending` is the chunks still to fetch, in ascending order, and `lanes`
    /// how many ways to divide them up front.
    ///
    /// Dividing at the start rather than letting the first lane take the lot
    /// and the others steal it back: both end up balanced, but only this one
    /// starts balanced, and a transfer that is over in four chunks never gets
    /// the chance to rebalance.
    pub fn new(pending: Vec<u64>, lanes: usize) -> Self {
        let mut free = Vec::new();
        if !pending.is_empty() {
            let ways = lanes.clamp(1, pending.len());
            let each = pending.len() / ways;
            let spare = pending.len() % ways;
            let mut start = 0;
            for lane in 0..ways {
                // The remainder goes to the earliest runs rather than all to
                // the last one, which would leave one lane with a run the
                // length of the shortfall.
                let end = start + each + usize::from(lane < spare);
                free.push(Span { start, end });
                start = end;
            }
        }
        Self { inner: Mutex::new(Inner { pending, free, ..Default::default() }) }
    }

    /// The next chunk for this lane, or `None` when there is nothing left that
    /// it can take without racing another lane for it.
    pub fn take(&self, lane: usize) -> Option<u64> {
        let mut inner = self.inner.lock().unwrap();
        // Before anything else: a chunk somebody handed back is work that is
        // already overdue.
        if let Some(chunk) = inner.orphans.pop() {
            return Some(chunk);
        }

        if let Some(span) = inner.claims.get_mut(&lane)
            && !span.is_empty()
        {
            let at = span.start;
            span.start += 1;
            return inner.pending.get(at).copied();
        }

        let span = inner.acquire()?;
        let at = span.start;
        inner.claims.insert(lane, Span { start: at + 1, end: span.end });
        inner.pending.get(at).copied()
    }

    /// Hand a chunk back without having fetched it.
    pub fn give_back(&self, chunk: u64) {
        self.inner.lock().unwrap().orphans.push(chunk);
    }

    /// This lane is out. Whatever it had left goes back on the pile.
    pub fn retire(&self, lane: usize) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(span) = inner.claims.remove(&lane)
            && !span.is_empty()
        {
            inner.free.push(span);
        }
    }

    /// Chunks still to be handed out. Anything already in flight is not
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

    fn regions(count: u64, lanes: usize) -> Regions {
        Regions::new((0..count).collect(), lanes)
    }

    /// Drain a lane until it has nothing of its own left.
    fn drain(r: &Regions, lane: usize, limit: usize) -> Vec<u64> {
        let mut taken = Vec::new();
        while taken.len() < limit {
            match r.take(lane) {
                Some(chunk) => taken.push(chunk),
                None => break,
            }
        }
        taken
    }

    #[test]
    fn one_lane_walks_the_file_in_order() {
        let r = regions(8, 1);
        assert_eq!(drain(&r, 0, 16), (0..8).collect::<Vec<_>>());
        assert!(r.is_empty());
    }

    #[test]
    fn two_lanes_work_on_separate_stretches() {
        // The point of the exercise: not four clients picking at the same
        // pile, but each one walking its own part.
        let r = regions(8, 2);
        assert_eq!(r.take(0), Some(0));
        assert_eq!(r.take(1), Some(4), "the second lane started inside the first's run");

        let first = drain(&r, 0, 3);
        let second = drain(&r, 1, 3);
        assert_eq!(first, vec![1, 2, 3]);
        assert_eq!(second, vec![5, 6, 7]);
    }

    #[test]
    fn a_lane_that_joins_late_is_given_a_stretch_of_its_own() {
        // A phone paired thirty seconds in has to start carrying bytes without
        // the transfer being restarted, and it takes the same path as a lane
        // that merely ran out early.
        let r = regions(16, 1);
        assert_eq!(drain(&r, 0, 4), vec![0, 1, 2, 3]);

        let joined = r.take(1).expect("the new lane gets work");
        assert!(joined >= 8, "the new lane started at {joined}, inside work already under way");
        assert!(joined < 16);
    }

    #[test]
    fn a_fast_lane_takes_over_from_a_slow_one_rather_than_waiting() {
        // Round robin would hand a slow lane a fixed share and let it decide
        // when the download finishes. Here the fast lane keeps taking halves
        // and the slow one is left with what it can manage.
        let r = regions(64, 2);
        // Both start; lane 1 then does nothing at all.
        let mut fast = vec![r.take(0).unwrap()];
        let slow_start = r.take(1).unwrap();

        while let Some(chunk) = r.take(0) {
            fast.push(chunk);
        }
        assert!(fast.len() > 50, "the idle lane kept {} chunks", 64 - fast.len());
        assert!(fast.iter().all(|c| *c != slow_start), "two lanes took the same chunk");
    }

    #[test]
    fn nothing_is_handed_out_twice() {
        let r = regions(50, 4);
        let mut seen = Vec::new();
        // Four lanes, taken from in a deliberately uneven order.
        for round in 0..60 {
            for lane in 0..4 {
                if round % (lane + 1) == 0
                    && let Some(chunk) = r.take(lane)
                {
                    seen.push(chunk);
                }
            }
        }
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), seen.len(), "a chunk was given to two lanes");
        assert_eq!(sorted, (0..50).collect::<Vec<_>>(), "some chunk was never handed out");
        assert!(r.is_empty());
    }

    #[test]
    fn a_chunk_handed_back_is_retried_before_anything_new() {
        // It is already overdue: burying it in some lane's run would leave it
        // until that lane walked to it, which may be the end of the file.
        let r = regions(32, 2);
        let first = r.take(0).unwrap();
        r.give_back(first);
        assert_eq!(r.take(1), Some(first), "the returned chunk was not picked up");
    }

    #[test]
    fn a_retired_lanes_stretch_goes_back_on_the_pile() {
        // An interface that drops must not take a quarter of the file with it.
        let r = regions(16, 2);
        r.take(0);
        r.take(1);
        let left = r.remaining();
        r.retire(1);
        assert_eq!(r.remaining(), left, "retiring lost work");

        let rest = drain(&r, 0, 32);
        assert_eq!(rest.len(), left, "lane 0 could not pick up what lane 1 left");
    }

    #[test]
    fn a_resumed_transfer_divides_only_what_is_missing() {
        // Positions, not chunk numbers: the journal's holes are not a range.
        let r = Regions::new(vec![3, 4, 9, 20, 21], 2);
        let mut all = drain(&r, 0, 8);
        all.extend(drain(&r, 1, 8));
        all.sort_unstable();
        assert_eq!(all, vec![3, 4, 9, 20, 21]);
    }

    #[test]
    fn a_single_chunk_is_not_split_out_from_under_the_lane_fetching_it() {
        // Halving a run of one would hand the same chunk to two lanes.
        let r = regions(2, 2);
        assert_eq!(r.take(0), Some(0));
        assert_eq!(r.take(1), Some(1));
        assert_eq!(r.take(2), None);
    }

    #[test]
    fn an_empty_file_hands_out_nothing() {
        let r = Regions::new(Vec::new(), 4);
        assert!(r.is_empty());
        assert_eq!(r.take(0), None);
    }
}
