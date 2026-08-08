//! Vector similarity: the metric, the `VectorIndex` seam, and an exact
//! brute-force implementation (arch/01 §5).
//!
//! The KV is the single source of truth for vectors (they live in node
//! records); an index is an accelerator built from them. [`BruteForceIndex`]
//! is both the small-plane implementation (arch/01 §5: below a threshold,
//! skip ANN) and the exact oracle that [`super::hnsw`]'s recall is tested
//! against.
//!
//! Everything here is **total** in the face of soft-schema data: a candidate
//! whose vector dimension doesn't match the query is simply skipped, never an
//! error — the same posture as the expression evaluator (arch/03 §2).

use std::collections::BinaryHeap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, backend};

/// Similarity metric. `distance` is "smaller = closer" (what the index ranks
/// by); `similarity` is "larger = more similar" (what the query score channel
/// exposes). The two are always monotonically opposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Metric {
    /// Cosine similarity in `[-1, 1]`; distance `1 - cos` in `[0, 2]`.
    Cosine,
    /// Dot product; distance is its negation.
    Dot,
    /// Euclidean (L2); distance is the L2 norm, similarity its negation.
    L2,
}

impl Metric {
    /// Distance between two equal-length vectors (smaller = closer).
    /// Mismatched dimensions yield `+∞` so the pair never ranks as close.
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return f32::INFINITY;
        }
        match self {
            Metric::Cosine => 1.0 - cosine(a, b),
            Metric::Dot => -dot(a, b),
            Metric::L2 => l2(a, b),
        }
    }

    /// Similarity (larger = more similar) — the score-channel value.
    pub fn similarity(self, a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() {
            return f32::NEG_INFINITY;
        }
        match self {
            Metric::Cosine => cosine(a, b),
            Metric::Dot => dot(a, b),
            Metric::L2 => -l2(a, b),
        }
    }

    /// Recover `similarity` from a `distance` this metric produced, without
    /// the vectors — the two are exact affine twins per metric. Lets a
    /// heap-ranked top-k (by distance) report true similarity scores.
    pub fn similarity_from_distance(self, distance: f32) -> f32 {
        match self {
            Metric::Cosine => 1.0 - distance, // distance = 1 - cos
            Metric::Dot | Metric::L2 => -distance,
        }
    }

    /// One-byte tag for storing a metric in the KV (index declarations).
    pub fn tag(self) -> u8 {
        match self {
            Metric::Cosine => 0,
            Metric::Dot => 1,
            Metric::L2 => 2,
        }
    }

    pub fn from_tag(tag: u8) -> Option<Metric> {
        match tag {
            0 => Some(Metric::Cosine),
            1 => Some(Metric::Dot),
            2 => Some(Metric::L2),
            _ => None,
        }
    }
}

/// Dot product — the one hot numeric kernel. Every metric reduces to it (see
/// `l2`/`cosine` below and [`super::hnsw`]'s cached-norm distances), so this is
/// where SIMD pays off across build, search, and brute force alike. Dispatches
/// to an AVX2+FMA path when the CPU supports it (x86-64), to NEON on aarch64
/// (baseline there — no runtime detection needed), else a scalar fallback.
pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            // SAFETY: gated on runtime detection of avx2+fma just above.
            return unsafe { dot_avx2(a, b) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        return dot_neon(a, b);
    }
    #[allow(unreachable_code)]
    dot_scalar(a, b)
}

fn dot_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Four independent accumulators, not one: a single `acc = fmadd(…, acc)`
/// chain serializes on FMA latency (~4 cycles) while the core can *issue* two
/// FMAs per cycle, capping the loop at a fraction of peak. Four chains keep
/// the FMA units fed; measured on 1024-dim vectors this is ~2.3× faster with
/// the operand in L1 and ~1.5× on cache-cold graph traversal (i9-14900HX).
/// Wider ISA is not the lever here — AVX-512 is absent on consumer Intel
/// (fused off since 12th gen) — the dependency chain is.
///
/// Reassociating the sum changes rounding order vs. a single chain; that is
/// fine because every caller (all metrics, HNSW and brute force alike) goes
/// through this same kernel, so distances stay mutually consistent.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn dot_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    unsafe {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let (mut a0, mut a1, mut a2, mut a3) = (
            _mm256_setzero_ps(),
            _mm256_setzero_ps(),
            _mm256_setzero_ps(),
            _mm256_setzero_ps(),
        );
        let mut i = 0;
        // Main loop: 32 floats per iteration, one FMA per accumulator.
        while i + 32 <= n {
            a0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), a0);
            a1 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
                a1,
            );
            a2 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 16)),
                _mm256_loadu_ps(pb.add(i + 16)),
                a2,
            );
            a3 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 24)),
                _mm256_loadu_ps(pb.add(i + 24)),
                a3,
            );
            i += 32;
        }
        // Fold the four chains, then mop up any full 8-lane blocks (dims not
        // divisible by 32, e.g. 24 or 40).
        let mut acc = _mm256_add_ps(_mm256_add_ps(a0, a1), _mm256_add_ps(a2, a3));
        while i + 8 <= n {
            acc = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc);
            i += 8;
        }
        // Horizontal sum of the 8 lanes.
        let mut lanes = [0f32; 8];
        _mm256_storeu_ps(lanes.as_mut_ptr(), acc);
        let mut sum: f32 = lanes.iter().sum();
        // Scalar tail for the remaining < 8 elements.
        while i < n {
            sum += *pa.add(i) * *pb.add(i);
            i += 1;
        }
        sum
    }
}

/// NEON twin of [`dot_avx2`] for aarch64 (Apple Silicon, Graviton, …), where
/// the scalar fallback would otherwise run one serial FMA per element. NEON is
/// baseline on aarch64, so no runtime detection — the `cfg` alone gates it.
///
/// Same dependency-chain reasoning as the AVX2 kernel, scaled to the ISA:
/// NEON registers are 128-bit (4×f32), and Apple M-series cores execute four
/// FMA pipes at ~3–4 cycle latency, so it takes MORE independent chains to
/// keep them fed, not fewer. Eight accumulators (32 floats/iteration, the same
/// stride as the AVX2 path) is a reasonable static choice; final tuning wants
/// a measurement on real hardware (the `simd_kernels_match_scalar_reference`
/// test guards correctness on any aarch64 machine that runs the suite).
#[cfg(target_arch = "aarch64")]
fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::aarch64::*;
    // SAFETY: NEON is part of the aarch64 baseline; the pointer arithmetic
    // stays within `min(a.len(), b.len())` == n (caller guarantees equal
    // lengths; `debug_assert` in `dot`).
    unsafe {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = [vdupq_n_f32(0.0); 8];
        let mut i = 0;
        // Main loop: 32 floats per iteration, one FMA per accumulator chain.
        while i + 32 <= n {
            for (j, chain) in acc.iter_mut().enumerate() {
                let o = i + j * 4;
                *chain = vfmaq_f32(*chain, vld1q_f32(pa.add(o)), vld1q_f32(pb.add(o)));
            }
            i += 32;
        }
        // Fold the chains, then mop up any full 4-lane blocks.
        let mut folded = vaddq_f32(
            vaddq_f32(vaddq_f32(acc[0], acc[1]), vaddq_f32(acc[2], acc[3])),
            vaddq_f32(vaddq_f32(acc[4], acc[5]), vaddq_f32(acc[6], acc[7])),
        );
        while i + 4 <= n {
            folded = vfmaq_f32(folded, vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            i += 4;
        }
        // Horizontal sum of the 4 lanes, then the scalar tail.
        let mut sum = vaddvq_f32(folded);
        while i < n {
            sum += *pa.add(i) * *pb.add(i);
            i += 1;
        }
        sum
    }
}

/// L2 distance — like [`dot`], a hot kernel on the brute-force paths, so it
/// gets the same SIMD treatment. Kept as a dedicated sum-of-squared-differences
/// pass (subtract feeding FMA) rather than the `√(‖a‖²+‖b‖²−2·a·b)` identity:
/// that would take three passes over memory instead of one and loses precision
/// to cancellation exactly when vectors are close — the case that decides
/// nearest-neighbor ranking.
fn l2(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            // SAFETY: gated on runtime detection of avx2+fma just above.
            return unsafe { l2sq_avx2(a, b) }.sqrt();
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        return l2sq_neon(a, b).sqrt();
    }
    #[allow(unreachable_code)]
    l2sq_scalar(a, b).sqrt()
}

fn l2sq_scalar(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// Sum of squared differences, AVX2+FMA — the same four-chain structure as
/// [`dot_avx2`] (see there for the dependency-chain rationale), with a
/// subtract feeding each FMA: `acc += d·d` where `d = a−b`.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn l2sq_avx2(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::x86_64::*;
    unsafe {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let (mut a0, mut a1, mut a2, mut a3) = (
            _mm256_setzero_ps(),
            _mm256_setzero_ps(),
            _mm256_setzero_ps(),
            _mm256_setzero_ps(),
        );
        let mut i = 0;
        while i + 32 <= n {
            let d0 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            let d1 = _mm256_sub_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
            );
            let d2 = _mm256_sub_ps(
                _mm256_loadu_ps(pa.add(i + 16)),
                _mm256_loadu_ps(pb.add(i + 16)),
            );
            let d3 = _mm256_sub_ps(
                _mm256_loadu_ps(pa.add(i + 24)),
                _mm256_loadu_ps(pb.add(i + 24)),
            );
            a0 = _mm256_fmadd_ps(d0, d0, a0);
            a1 = _mm256_fmadd_ps(d1, d1, a1);
            a2 = _mm256_fmadd_ps(d2, d2, a2);
            a3 = _mm256_fmadd_ps(d3, d3, a3);
            i += 32;
        }
        let mut acc = _mm256_add_ps(_mm256_add_ps(a0, a1), _mm256_add_ps(a2, a3));
        while i + 8 <= n {
            let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            acc = _mm256_fmadd_ps(d, d, acc);
            i += 8;
        }
        let mut lanes = [0f32; 8];
        _mm256_storeu_ps(lanes.as_mut_ptr(), acc);
        let mut sum: f32 = lanes.iter().sum();
        while i < n {
            let d = *pa.add(i) - *pb.add(i);
            sum += d * d;
            i += 1;
        }
        sum
    }
}

/// Sum of squared differences, NEON — the eight-chain structure of
/// [`dot_neon`] (see there for the pipe/latency rationale) with a subtract
/// feeding each FMA.
#[cfg(target_arch = "aarch64")]
fn l2sq_neon(a: &[f32], b: &[f32]) -> f32 {
    use std::arch::aarch64::*;
    // SAFETY: NEON is part of the aarch64 baseline; pointer arithmetic stays
    // within `n` (caller guarantees equal lengths; `debug_assert` in `l2`).
    unsafe {
        let n = a.len();
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = [vdupq_n_f32(0.0); 8];
        let mut i = 0;
        while i + 32 <= n {
            for (j, chain) in acc.iter_mut().enumerate() {
                let o = i + j * 4;
                let d = vsubq_f32(vld1q_f32(pa.add(o)), vld1q_f32(pb.add(o)));
                *chain = vfmaq_f32(*chain, d, d);
            }
            i += 32;
        }
        let mut folded = vaddq_f32(
            vaddq_f32(vaddq_f32(acc[0], acc[1]), vaddq_f32(acc[2], acc[3])),
            vaddq_f32(vaddq_f32(acc[4], acc[5]), vaddq_f32(acc[6], acc[7])),
        );
        while i + 4 <= n {
            let d = vsubq_f32(vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            folded = vfmaq_f32(folded, d, d);
            i += 4;
        }
        let mut sum = vaddvq_f32(folded);
        while i < n {
            let d = *pa.add(i) - *pb.add(i);
            sum += d * d;
            i += 1;
        }
        sum
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let na = dot(a, a).sqrt();
    let nb = dot(b, b).sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        (dot(a, b) / (na * nb)).clamp(-1.0, 1.0)
    }
}

/// Restricts a search to a candidate id set — how a graph frontier or label
/// predicate is pushed into vector search (arch/03 §4.3, §4.6).
pub trait IdFilter {
    fn contains(&self, id: u64) -> bool;
}

impl IdFilter for std::collections::HashSet<u64> {
    fn contains(&self, id: u64) -> bool {
        std::collections::HashSet::contains(self, &id)
    }
}

/// A vector search accelerator over a set of `(id, vector)` pairs.
pub trait VectorIndex {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()>;
    fn remove(&mut self, id: u64) -> Result<()>;

    /// Up to `k` nearest ids to `query`, ascending by distance. `filter`, if
    /// given, restricts results to matching ids.
    fn search(&self, query: &[f32], k: usize, filter: Option<&dyn IdFilter>) -> Result<Vec<Hit>>;

    fn persist(&self, path: &Path) -> Result<()>;
}

/// One search result: an id and its distance (smaller = closer).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub id: u64,
    pub distance: f32,
}

/// A max-heap entry for top-k: ordered by distance via `f32::total_cmp` (so
/// NaN is well-ordered rather than panicking). A bounded max-heap keeps the
/// `k` smallest distances by popping the current largest when it overflows.
#[derive(PartialEq)]
struct HeapItem {
    distance: f32,
    id: u64,
}
impl Eq for HeapItem {}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance.total_cmp(&other.distance)
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Collects the `k` smallest-distance items from an `(id, distance)` stream.
/// Shared by brute force and HNSW's layer-0 result gathering.
pub(crate) fn top_k(items: impl Iterator<Item = (u64, f32)>, k: usize) -> Vec<Hit> {
    if k == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::with_capacity(k + 1);
    for (id, distance) in items {
        heap.push(HeapItem { distance, id });
        if heap.len() > k {
            heap.pop(); // drop the current farthest
        }
    }
    let mut hits: Vec<Hit> = heap
        .into_iter()
        .map(|h| Hit {
            id: h.id,
            distance: h.distance,
        })
        .collect();
    hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
    hits
}

/// Exact nearest-neighbour index: stores every vector, scans them all per
/// query. Exact and dependency-free; the correctness oracle for HNSW and the
/// implementation used for planes too small to be worth an ANN graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BruteForceIndex {
    metric: Metric,
    vectors: Vec<(u64, Vec<f32>)>,
}

impl BruteForceIndex {
    pub fn new(metric: Metric) -> Self {
        Self {
            metric,
            vectors: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vectors.is_empty()
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        postcard::from_bytes(&bytes).map_err(backend)
    }
}

impl VectorIndex for BruteForceIndex {
    fn insert(&mut self, id: u64, vector: &[f32]) -> Result<()> {
        // Overwrite any existing vector for this id (idempotent upsert).
        if let Some(slot) = self.vectors.iter_mut().find(|(x, _)| *x == id) {
            slot.1 = vector.to_vec();
        } else {
            self.vectors.push((id, vector.to_vec()));
        }
        Ok(())
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        self.vectors.retain(|(x, _)| *x != id);
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize, filter: Option<&dyn IdFilter>) -> Result<Vec<Hit>> {
        let items = self
            .vectors
            .iter()
            .filter(|(id, _)| filter.is_none_or(|f| f.contains(*id)))
            .map(|(id, v)| (*id, self.metric.distance(query, v)));
        Ok(top_k(items, k))
    }

    fn persist(&self, path: &Path) -> Result<()> {
        let bytes = postcard::to_stdvec(self).map_err(backend)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn metric_ranks_closest_first() {
        // cosine: [1,0] is closer to [1,0] than to [0,1]
        assert!(Metric::Cosine.distance(&[1.0, 0.0], &[1.0, 0.0]) < 1e-6);
        assert!(
            Metric::Cosine.distance(&[1.0, 0.0], &[1.0, 0.0])
                < Metric::Cosine.distance(&[1.0, 0.0], &[0.0, 1.0])
        );
        // similarity is opposed to distance
        assert!(
            Metric::Cosine.similarity(&[1.0, 0.0], &[1.0, 0.0])
                > Metric::Cosine.similarity(&[1.0, 0.0], &[0.0, 1.0])
        );
        // L2 of identical vectors is 0
        assert_eq!(Metric::L2.distance(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 0.0);
        // dot distance is negated dot
        assert_eq!(Metric::Dot.distance(&[1.0, 2.0], &[3.0, 4.0]), -(3.0 + 8.0));
    }

    /// Pins the SIMD kernels (`dot`, `l2`) against their scalar references
    /// across dimensions that exercise every phase: the 32-wide main loop, the
    /// narrower mop-up, the scalar tail, and sub-SIMD inputs. Tolerance, not
    /// equality: the multi-accumulator reassociation legitimately rounds
    /// differently from the scalar left-fold. On aarch64 the same assertions
    /// exercise the NEON kernels instead.
    #[test]
    fn simd_kernels_match_scalar_reference() {
        let mut seed = 0xD07_0D07_0D07u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f32 / (1u32 << 24) as f32) - 0.5
        };
        for n in [0, 1, 3, 7, 8, 9, 24, 31, 32, 33, 40, 100, 1024, 1536] {
            let a: Vec<f32> = (0..n).map(|_| rnd()).collect();
            let b: Vec<f32> = (0..n).map(|_| rnd()).collect();
            // Relative-ish bound: n float ops each rounding at ~1e-7.
            let tol = 1e-5 * (n.max(1) as f32);

            let (simd, scalar) = (dot(&a, &b), dot_scalar(&a, &b));
            assert!(
                (simd - scalar).abs() <= tol,
                "dot dim {n}: simd {simd} vs scalar {scalar}"
            );

            let (simd, scalar) = (l2(&a, &b), l2sq_scalar(&a, &b).sqrt());
            assert!(
                (simd - scalar).abs() <= tol,
                "l2 dim {n}: simd {simd} vs scalar {scalar}"
            );
        }
    }

    #[test]
    fn dimension_mismatch_is_far_not_a_panic() {
        assert_eq!(Metric::Cosine.distance(&[1.0], &[1.0, 2.0]), f32::INFINITY);
        assert_eq!(
            Metric::L2.similarity(&[1.0], &[1.0, 2.0]),
            f32::NEG_INFINITY
        );
    }

    fn build(metric: Metric, vecs: &[(u64, &[f32])]) -> BruteForceIndex {
        let mut idx = BruteForceIndex::new(metric);
        for (id, v) in vecs {
            idx.insert(*id, v).unwrap();
        }
        idx
    }

    #[test]
    fn brute_force_topk_orders_by_distance() {
        let idx = build(
            Metric::L2,
            &[
                (1, &[0.0, 0.0]),
                (2, &[1.0, 0.0]),
                (3, &[5.0, 5.0]),
                (4, &[0.5, 0.0]),
            ],
        );
        let hits = idx.search(&[0.0, 0.0], 2, None).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 4]);
        // distances ascending
        assert!(hits[0].distance <= hits[1].distance);
    }

    #[test]
    fn brute_force_filter_restricts_candidates() {
        let idx = build(Metric::L2, &[(1, &[0.0]), (2, &[1.0]), (3, &[2.0])]);
        let allow: HashSet<u64> = [2, 3].into_iter().collect();
        let hits = idx.search(&[0.0], 5, Some(&allow)).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn insert_overwrites_and_remove_deletes() {
        let mut idx = build(Metric::L2, &[(1, &[0.0]), (2, &[9.0])]);
        idx.insert(2, &[0.1]).unwrap(); // move 2 close to origin
        assert_eq!(idx.len(), 2);
        let hits = idx.search(&[0.0], 2, None).unwrap();
        assert_eq!(hits[0].id, 1);
        assert_eq!(hits[1].id, 2);
        idx.remove(1).unwrap();
        let hits = idx.search(&[0.0], 5, None).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn k_zero_and_k_larger_than_set() {
        let idx = build(Metric::L2, &[(1, &[0.0]), (2, &[1.0])]);
        assert!(idx.search(&[0.0], 0, None).unwrap().is_empty());
        assert_eq!(idx.search(&[0.0], 99, None).unwrap().len(), 2);
    }

    #[test]
    fn persist_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf.idx");
        let idx = build(Metric::Cosine, &[(1, &[1.0, 0.0]), (2, &[0.0, 1.0])]);
        idx.persist(&path).unwrap();
        let loaded = BruteForceIndex::load(&path).unwrap();
        assert_eq!(
            loaded.search(&[1.0, 0.0], 1, None).unwrap()[0].id,
            idx.search(&[1.0, 0.0], 1, None).unwrap()[0].id
        );
    }
}
