//! The persistent, cross-query graph cache (arch/02 §3–4): a `Database`-level
//! moka W-TinyLFU cache of decoded records + adjacency segments, shared by
//! every query.
//!
//! **Coherence** is coarse and lock-free (arch/02 §3, invalidate-only). Every
//! entry is stamped with the [`commit_seq`](crate::storage::graph::read_commit_seq)
//! it was read at. A reader pinned to snapshot `seq` serves an entry only when
//! `entry.seq == seq`; any write bumps the sequence, so all prior entries are
//! silently stale for later snapshots (a logical flush) and moka evicts them in
//! due course. Because the seq lives in the KV, a reader's `seq` always matches
//! the storage snapshot it sees — no version chains, no locks, no races.
//!
//! The one exception is a *foreign* sequence — snapshot restore and
//! replication land a seq rather than bumping one, so the same number can
//! stamp two different states. Those paths [`invalidate_all`], and because
//! a reader that opened its snapshot *before* such a commit could still
//! insert pre-restore data afterwards, stamped with the very seq the restore
//! landed, the cache also keeps a [`generation`]: a reader captures it
//! before opening its snapshot, `invalidate_all` bumps it first, and an
//! insert from an older generation is refused. A lost insert is only a
//! miss; a stale one would be served to every later reader at that seq.
//!
//! [`invalidate_all`]: GraphCache::invalidate_all
//! [`generation`]: GraphCache::generation
//! Keys are `(plane, id)`. Node/edge ids are globally unique (arch/02 §1), so
//! one cache serves all planes without collisions — but a lookup is *scoped*
//! to a plane: `get_node(plane B, id in A)` is `None`, and the cache must say
//! the same, so the plane is part of the key rather than checked on hit.
//! Keying (not checking) is what keeps a per-plane miss a plain miss: a hit
//! under plane A never shadows the answer for plane B. Only *existing* records
//! are cached (no negative caching yet — arch/02 §7.4); adjacency is always
//! cached, empty slice included.
//!
//! **Per-entry cap** (arch/02 §4): an entry heavier than [`ENTRY_CAP_BYTES`]
//! — a hub's adjacency, a record carrying a huge blob — bypasses the L2. It
//! would evict a whole working set to hold one thing that decodes once per
//! query anyway; the executor streams it from storage instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use moka::sync::Cache;

use crate::types::{Dir, EdgeRecord, Neighbor, NodeRecord, PlaneId, PropValue, Properties};

/// Heaviest entry the L2 will hold, in the weigher's units (arch/02 §4). 4 MiB
/// is 1/16 of the default 64 MiB budget: a hub with ~250k neighbours, or a
/// record with a ~4 MiB blob — either is decode-once-per-query territory, not
/// working-set territory. Bounding the ratio (rather than the absolute size)
/// keeps one entry from ever being most of the cache.
const ENTRY_CAP_BYTES: u32 = 4 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Node(PlaneId, u64),
    Edge(PlaneId, u64),
    Adj(PlaneId, u64, Dir, Option<String>),
}

/// A decoded payload tagged with the commit seq it is valid at (arch/02 §3).
#[derive(Clone)]
struct Stamped {
    seq: u64,
    payload: Payload,
}

#[derive(Clone)]
enum Payload {
    Node(Arc<NodeRecord>),
    Edge(Arc<EdgeRecord>),
    Adj(Arc<[Neighbor]>),
}

/// Shared decoded-object cache. Cheap to hold behind `&` (moka is internally
/// `Arc`-shared and thread-safe), so it lives in `Database` and every query's
/// reader borrows it.
pub(crate) struct GraphCache {
    cache: Cache<Key, Stamped>,
    /// Entries weighing more than this are not inserted (arch/02 §4).
    entry_cap: u32,
    /// Bumped by every `invalidate_all`; an insert carrying an older value
    /// is refused (see the module docs).
    generation: AtomicU64,
}

impl GraphCache {
    /// A cache bounded to roughly `max_bytes` of decoded payload (arch/02 §4).
    pub fn new(max_bytes: u64) -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|_k, v: &Stamped| weight(v))
                .build(),
            // A tiny budget (tests, constrained hosts) shrinks the cap with it,
            // so one entry can never be more than a sixteenth of the cache.
            entry_cap: ENTRY_CAP_BYTES.min((max_bytes / 16).max(1).min(u32::MAX as u64) as u32),
            generation: AtomicU64::new(0),
        }
    }

    /// The token a reader must capture *before* it opens its read snapshot
    /// and hand back with every `put_*`; see the module docs.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Insert unless the entry is over the per-entry cap (arch/02 §4), in
    /// which case it simply isn't cached and the next reader decodes it
    /// again, or the reader's `generation` is behind — its snapshot may
    /// predate a foreign-seq commit that has since flushed the cache, so
    /// what it read cannot be trusted under the seq it would stamp.
    fn insert(&self, generation: u64, key: Key, value: Stamped) {
        if weight(&value) <= self.entry_cap && generation == self.generation() {
            self.cache.insert(key, value);
        }
    }

    /// Drop every entry, whatever its stamp. For the paths that land a commit
    /// sequence the cache may already have stamped with *different* data —
    /// snapshot restore and replication set the sequence to a foreign value
    /// rather than bumping it, so exact-seq matching alone cannot tell the old
    /// entries from the new state (arch/02 §3).
    ///
    /// The generation moves first: an insert that races this call is either
    /// refused (it saw the new generation) or wiped (it landed before the
    /// clear). The other order would let one slip in between.
    pub fn invalidate_all(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cache.invalidate_all();
    }

    pub fn node(&self, plane: PlaneId, id: u64, seq: u64) -> Option<Arc<NodeRecord>> {
        match self.cache.get(&Key::Node(plane, id)) {
            Some(Stamped {
                seq: s,
                payload: Payload::Node(n),
            }) if s == seq => Some(n),
            _ => None,
        }
    }

    pub fn put_node(
        &self,
        generation: u64,
        plane: PlaneId,
        id: u64,
        seq: u64,
        node: Arc<NodeRecord>,
    ) {
        self.insert(
            generation,
            Key::Node(plane, id),
            Stamped {
                seq,
                payload: Payload::Node(node),
            },
        );
    }

    pub fn edge(&self, plane: PlaneId, id: u64, seq: u64) -> Option<Arc<EdgeRecord>> {
        match self.cache.get(&Key::Edge(plane, id)) {
            Some(Stamped {
                seq: s,
                payload: Payload::Edge(e),
            }) if s == seq => Some(e),
            _ => None,
        }
    }

    pub fn put_edge(
        &self,
        generation: u64,
        plane: PlaneId,
        id: u64,
        seq: u64,
        edge: Arc<EdgeRecord>,
    ) {
        self.insert(
            generation,
            Key::Edge(plane, id),
            Stamped {
                seq,
                payload: Payload::Edge(edge),
            },
        );
    }

    pub fn adj(
        &self,
        plane: PlaneId,
        id: u64,
        dir: Dir,
        ty: Option<&str>,
        seq: u64,
    ) -> Option<Arc<[Neighbor]>> {
        match self
            .cache
            .get(&Key::Adj(plane, id, dir, ty.map(str::to_string)))
        {
            Some(Stamped {
                seq: s,
                payload: Payload::Adj(a),
            }) if s == seq => Some(a),
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)] // (generation, key parts, stamp, payload)
    pub fn put_adj(
        &self,
        generation: u64,
        plane: PlaneId,
        id: u64,
        dir: Dir,
        ty: Option<&str>,
        seq: u64,
        adj: Arc<[Neighbor]>,
    ) {
        self.insert(
            generation,
            Key::Adj(plane, id, dir, ty.map(str::to_string)),
            Stamped {
                seq,
                payload: Payload::Adj(adj),
            },
        );
    }

    /// Approximate total decoded bytes held (moka's weighted size).
    #[cfg(test)]
    pub fn weighted_size(&self) -> u64 {
        self.cache.run_pending_tasks();
        self.cache.weighted_size()
    }
}

/// Rough heap-size estimate for the byte budget (arch/02 §4: "approximate heap
/// size"). Shallow — good enough for eviction, cheap on the miss path.
fn weight(v: &Stamped) -> u32 {
    let body = match &v.payload {
        Payload::Node(n) => {
            64 + n.labels.iter().map(|l| l.len() + 16).sum::<usize>() + props(&n.properties)
        }
        Payload::Edge(e) => 64 + e.ty.len() + props(&e.properties),
        Payload::Adj(a) => 16 + a.len() * 16, // Neighbor = 2×u64
    };
    body.min(u32::MAX as usize) as u32
}

fn props(p: &Properties) -> usize {
    p.iter()
        .map(|(k, d)| {
            let v = match &d.value {
                PropValue::Str(s) => s.len(),
                PropValue::Bytes(b) => b.len(),
                PropValue::Vector(v) => v.len() * 4,
                PropValue::List(l) => l.len() * 8,
                _ => 8,
            };
            k.len() + v + d.description.as_ref().map_or(0, |s| s.len()) + 32
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(plane: PlaneId) -> Arc<NodeRecord> {
        Arc::new(NodeRecord {
            id: crate::types::NodeId(1),
            plane,
            external_key: None,
            labels: vec!["N".into()],
            properties: Properties::new(),
        })
    }

    #[test]
    fn a_hit_is_scoped_to_the_plane_it_was_read_in() {
        let cache = GraphCache::new(1 << 20);
        let a = PlaneId(1);
        let b = PlaneId(2);
        let g = cache.generation();
        cache.put_node(g, a, 1, 5, record(a));
        cache.put_adj(g, a, 1, Dir::Out, None, 5, Arc::from(vec![]));

        assert!(cache.node(a, 1, 5).is_some(), "the plane that read it hits");
        // Ids are global, so the same id looked up from another plane must be
        // the miss storage would report — never plane A's record.
        assert!(cache.node(b, 1, 5).is_none());
        assert!(cache.adj(b, 1, Dir::Out, None, 5).is_none());
    }

    #[test]
    fn oversized_entries_bypass_the_cache() {
        // 1 MiB budget ⇒ 64 KiB per-entry cap; 16 bytes per neighbour.
        let cache = GraphCache::new(1 << 20);
        let plane = PlaneId(1);
        let hub: Arc<[Neighbor]> = (0..10_000u64)
            .map(|i| Neighbor {
                node: crate::types::NodeId(i),
                edge: crate::types::EdgeId(i),
            })
            .collect::<Vec<_>>()
            .into();
        let g = cache.generation();
        cache.put_adj(g, plane, 1, Dir::Out, None, 3, hub);
        assert!(
            cache.adj(plane, 1, Dir::Out, None, 3).is_none(),
            "a hub over the cap is streamed from storage, not cached"
        );
        assert_eq!(cache.weighted_size(), 0);

        let small: Arc<[Neighbor]> = Arc::from(vec![]);
        cache.put_adj(g, plane, 2, Dir::Out, None, 3, small);
        assert!(cache.adj(plane, 2, Dir::Out, None, 3).is_some());
    }

    #[test]
    fn invalidate_all_forgets_every_stamp() {
        let cache = GraphCache::new(1 << 20);
        let plane = PlaneId(1);
        cache.put_node(cache.generation(), plane, 1, 5, record(plane));
        cache.invalidate_all();
        assert!(
            cache.node(plane, 1, 5).is_none(),
            "same seq, but restored data"
        );
    }

    #[test]
    fn an_insert_from_before_an_invalidation_is_refused() {
        // A reader that opened its snapshot before a restore or replicated
        // batch landed may finish after the cache was cleared. Whatever it
        // read belongs to the old state, yet it would stamp it with the seq
        // the foreign commit reused — so its generation, captured before the
        // snapshot, no longer matches and the insert is dropped.
        let cache = GraphCache::new(1 << 20);
        let plane = PlaneId(1);
        let before = cache.generation();
        cache.invalidate_all();
        cache.put_node(before, plane, 1, 5, record(plane));
        cache.put_adj(before, plane, 1, Dir::Out, None, 5, Arc::from(vec![]));
        assert!(
            cache.node(plane, 1, 5).is_none(),
            "stale-generation node refused"
        );
        assert!(
            cache.adj(plane, 1, Dir::Out, None, 5).is_none(),
            "stale-generation adjacency refused"
        );

        // A reader that captured the generation after the clear is current.
        let after = cache.generation();
        assert_ne!(before, after);
        cache.put_node(after, plane, 1, 5, record(plane));
        assert!(cache.node(plane, 1, 5).is_some());
    }
}
