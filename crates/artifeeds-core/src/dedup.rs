//! Bounded duplicate suppression.
//!
//! Two levels:
//!
//! 1. **Per-source** `(source, uid_hash)` — the same item re-appearing in a
//!    feed we already ingested. Extremely common: most feeds repeat their last
//!    N entries forever. Checked in memory *before* any SQLite work.
//! 2. **Content hash** — the same article syndicated through several feeds
//!    (aggregators, "planet" blogs). The later copies are dropped from the
//!    in-memory window so the reader does not show it five times.
//!
//! Both are bounded FIFO rings: memory is a constant `capacity × 8 B` per
//! source no matter how long the app runs.

use crate::model::{Article, SourceId};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Debug)]
struct RingSet {
    set: FxHashSet<u64>,
    ring: VecDeque<u64>,
    cap: usize,
}

impl RingSet {
    fn new(cap: usize) -> Self {
        Self {
            set: FxHashSet::with_capacity_and_hasher(cap, Default::default()),
            ring: VecDeque::with_capacity(cap),
            cap,
        }
    }

    /// Returns `true` when the value was not already present.
    #[inline]
    fn insert(&mut self, value: u64) -> bool {
        if !self.set.insert(value) {
            return false;
        }
        self.ring.push_back(value);
        if self.ring.len() > self.cap
            && let Some(old) = self.ring.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }

    #[inline]
    fn contains(&self, value: u64) -> bool {
        self.set.contains(&value)
    }
}

/// A source's known uids, handed to a fetch that has to decide before it inserts.
pub type KnownUids = FxHashSet<u64>;

/// Duplicate index owned by the scheduler loop, so it needs no locking at all.
#[derive(Debug)]
pub struct DedupIndex {
    per_source: FxHashMap<SourceId, RingSet>,
    content: RingSet,
    per_source_cap: usize,
}

impl DedupIndex {
    /// `per_source_cap`: recent uids remembered per feed.
    /// `content_cap`: recent article bodies remembered globally.
    pub fn new(per_source_cap: usize, content_cap: usize) -> Self {
        Self {
            per_source: FxHashMap::default(),
            content: RingSet::new(content_cap.max(1024)),
            per_source_cap: per_source_cap.max(64),
        }
    }

    /// Warm the index from persisted history so a restart does not re-ingest
    /// (and re-surface) everything.
    pub fn seed(&mut self, articles: &[Arc<Article>]) {
        for a in articles {
            self.uid_seen(a.source, a.uid_hash);
            self.content.insert(a.content_hash);
        }
    }

    /// Returns `true` when this uid is new for the source.
    #[inline]
    pub fn uid_new(&mut self, source: SourceId, uid_hash: u64) -> bool {
        self.per_source
            .entry(source)
            .or_insert_with(|| RingSet::new(self.per_source_cap))
            .insert(uid_hash)
    }

    #[inline]
    pub fn uid_seen(&mut self, source: SourceId, uid_hash: u64) {
        self.uid_new(source, uid_hash);
    }

    #[inline]
    /// A snapshot of the uids this source has already produced.
    ///
    /// Used where the decision must be made *before* the items exist: a Zhihu
    /// collection's bodies have to be fetched in the same pass that inserts the
    /// rows, because an existing article is never updated (`DO NOTHING`), so a
    /// body missed on the first sighting is a body lost.
    pub fn known_uids(&self, source: SourceId) -> KnownUids {
        self.per_source
            .get(&source)
            .map(|ring| ring.set.clone())
            .unwrap_or_default()
    }

    pub fn uid_known(&self, source: SourceId, uid_hash: u64) -> bool {
        self.per_source.get(&source).is_some_and(|r| r.contains(uid_hash))
    }

    /// Returns `true` when this content hash has not been seen before.
    #[inline]
    pub fn content_new(&mut self, hash: u64) -> bool {
        self.content.insert(hash)
    }

    /// Forget a source entirely (it was removed from the config).
    pub fn forget_source(&mut self, source: SourceId) {
        self.per_source.remove(&source);
    }

    pub fn tracked_sources(&self) -> usize {
        self.per_source.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_source_ring_is_bounded() {
        let mut d = DedupIndex::new(64, 64);
        for i in 0..10_000u64 {
            assert!(d.uid_new(0, i));
        }
        assert!(d.uid_known(0, 9_999));
        assert!(!d.uid_known(0, 1), "oldest entries must have been evicted");
    }

    #[test]
    fn duplicate_uid_is_reported_once() {
        let mut d = DedupIndex::new(64, 64);
        assert!(d.uid_new(1, 42));
        assert!(!d.uid_new(1, 42));
        assert!(d.uid_new(2, 42), "different source, same uid hash");
    }

    #[test]
    fn content_dedup_is_global() {
        let mut d = DedupIndex::new(64, 1024);
        assert!(d.content_new(7));
        assert!(!d.content_new(7));
    }
}
