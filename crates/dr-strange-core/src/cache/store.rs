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
//! Keys are the globally-unique node/edge ids (arch/02 §1: ids never collide
//! across planes), so one cache serves all planes. Only *existing* records are
//! cached (no negative caching yet — arch/02 §7.4); adjacency is always cached,
//! empty slice included.

use std::sync::Arc;

use moka::sync::Cache;

use crate::types::{Dir, EdgeRecord, Neighbor, NodeRecord, PropValue, Properties};

#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Node(u64),
    Edge(u64),
    Adj(u64, Dir, Option<String>),
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
}

impl GraphCache {
    /// A cache bounded to roughly `max_bytes` of decoded payload (arch/02 §4).
    pub fn new(max_bytes: u64) -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(max_bytes)
                .weigher(|_k, v: &Stamped| weight(v))
                .build(),
        }
    }

    pub fn node(&self, id: u64, seq: u64) -> Option<Arc<NodeRecord>> {
        match self.cache.get(&Key::Node(id)) {
            Some(Stamped {
                seq: s,
                payload: Payload::Node(n),
            }) if s == seq => Some(n),
            _ => None,
        }
    }

    pub fn put_node(&self, id: u64, seq: u64, node: Arc<NodeRecord>) {
        self.cache.insert(
            Key::Node(id),
            Stamped {
                seq,
                payload: Payload::Node(node),
            },
        );
    }

    pub fn edge(&self, id: u64, seq: u64) -> Option<Arc<EdgeRecord>> {
        match self.cache.get(&Key::Edge(id)) {
            Some(Stamped {
                seq: s,
                payload: Payload::Edge(e),
            }) if s == seq => Some(e),
            _ => None,
        }
    }

    pub fn put_edge(&self, id: u64, seq: u64, edge: Arc<EdgeRecord>) {
        self.cache.insert(
            Key::Edge(id),
            Stamped {
                seq,
                payload: Payload::Edge(edge),
            },
        );
    }

    pub fn adj(&self, id: u64, dir: Dir, ty: Option<&str>, seq: u64) -> Option<Arc<[Neighbor]>> {
        match self.cache.get(&Key::Adj(id, dir, ty.map(str::to_string))) {
            Some(Stamped {
                seq: s,
                payload: Payload::Adj(a),
            }) if s == seq => Some(a),
            _ => None,
        }
    }

    pub fn put_adj(&self, id: u64, dir: Dir, ty: Option<&str>, seq: u64, adj: Arc<[Neighbor]>) {
        self.cache.insert(
            Key::Adj(id, dir, ty.map(str::to_string)),
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
