//! Version GC: which versions a compaction may drop, and which it must
//! keep because a reader or the retention window can still see them.

use super::*;

fn put(v: &str) -> Op {
    Op::Put(v.as_bytes().to_vec())
}

fn merged(entries: &[(u8, &str, u64, Op)]) -> BTreeMap<MemKey, Op> {
    entries
        .iter()
        .map(|(t, k, seq, op)| ((*t, k.as_bytes().to_vec(), Reverse(*seq)), op.clone()))
        .collect()
}

/// Run the streaming GC over an in-memory merged run and collect it.
fn gc(m: BTreeMap<MemKey, Op>, min_snapshot: u64) -> BTreeMap<MemKey, Op> {
    gc_versions(m.into_iter().map(Ok), min_snapshot)
        .collect::<Result<_>>()
        .unwrap()
}

fn seqs(out: &BTreeMap<MemKey, Op>, key: &str) -> Vec<u64> {
    out.keys()
        .filter(|(_, k, _)| k == key.as_bytes())
        .map(|(_, _, Reverse(s))| *s)
        .collect()
}

#[test]
fn keeps_versions_down_to_the_floor_and_drops_the_rest() {
    let m = merged(&[
        (0, "k", 9, put("v9")),
        (0, "k", 6, put("v6")),
        (0, "k", 4, put("v4")),
        (0, "k", 2, put("v2")),
    ]);
    let out = gc(m, 5);
    // Above the floor: 9, 6. The first at/below it (4) is what a reader
    // pinned at 5 sees, so it stays; 2 is unreachable.
    assert_eq!(seqs(&out, "k"), vec![9, 6, 4]);
    assert_eq!(out[&(0, b"k".to_vec(), Reverse(4))], put("v4"));
}

#[test]
fn a_lone_tombstone_below_the_floor_vanishes_but_a_shadowing_one_stays() {
    let m = merged(&[
        (0, "gone", 3, Op::Del),
        (0, "gone", 1, put("x")),
        (0, "live", 8, Op::Del),
        (0, "live", 7, put("y")),
    ]);
    let out = gc(m, 5);
    assert!(seqs(&out, "gone").is_empty(), "nothing older can resurface");
    // A tombstone above the floor still shadows the version a pinned
    // reader at 5 would otherwise see.
    assert_eq!(seqs(&out, "live"), vec![8, 7]);
}

#[test]
fn keys_are_grouped_per_table_and_every_survivor_keeps_its_value() {
    let m = merged(&[
        (0, "a", 2, put("n")),
        (1, "a", 2, put("e")),
        (1, "a", 1, put("old")),
    ]);
    let out = gc(m, 10);
    assert_eq!(out.len(), 2, "the same key in two tables is two keys");
    assert_eq!(out[&(0, b"a".to_vec(), Reverse(2))], put("n"));
    assert_eq!(out[&(1, b"a".to_vec(), Reverse(2))], put("e"));
}

#[test]
fn an_unbounded_retention_floor_of_zero_keeps_every_version() {
    // `retain_commits = 0` ⇒ `retention_floor` = 0 ⇒ compaction's floor is
    // 0. Sequences start at 1, so nothing is at/below it: every version
    // and every tombstone must survive, or time-travel to an old commit
    // would silently read the wrong value.
    let m = merged(&[
        (0, "k", 9, put("v9")),
        (0, "k", 6, put("v6")),
        (0, "k", 1, put("v1")),
        (0, "gone", 3, Op::Del),
        (0, "gone", 1, put("x")),
    ]);
    let before = m.clone();
    let out = gc(m, 0);
    assert_eq!(out, before, "floor 0 must be a no-op");
}

#[test]
fn the_streaming_merge_equals_the_map_merge_and_the_later_run_wins() {
    // Three runs with interleaved keys and a version spread; a key that
    // appears in every run; one (table, key, seq) duplicated across runs
    // to pin the "later run wins" rule the old overwrite gave for free.
    let runs = [
        merged(&[
            (0, "a", 1, put("a1")),
            (0, "c", 2, put("c2")),
            (1, "a", 3, put("ta3")),
            (0, "dup", 4, put("old")),
        ]),
        merged(&[
            (0, "a", 5, Op::Del),
            (0, "b", 6, put("b6")),
            (0, "dup", 4, put("new")),
        ]),
        merged(&[(0, "a", 7, put("a7")), (0, "c", 8, put("c8"))]),
    ];
    let mut expected = BTreeMap::new();
    for r in &runs {
        expected.extend(r.clone());
    }
    let streamed: Vec<(MemKey, Op)> =
        MergeIter::new(runs.iter().map(|r| r.clone().into_iter().map(Ok)))
            .collect::<Result<_>>()
            .unwrap();
    assert!(
        streamed.windows(2).all(|w| w[0].0 < w[1].0),
        "merge output must be strictly ordered"
    );
    let streamed: BTreeMap<MemKey, Op> = streamed.into_iter().collect();
    assert_eq!(streamed, expected);
    assert_eq!(streamed[&(0, b"dup".to_vec(), Reverse(4))], put("new"));
    // And the GC on top still sees whole version groups.
    let out = gc_versions(
        MergeIter::new(runs.iter().map(|r| r.clone().into_iter().map(Ok))),
        6,
    )
    .collect::<Result<BTreeMap<_, _>>>()
    .unwrap();
    // Table 0's "a": 7 above the floor, 5 is the floor version; table 1's
    // lone (1, "a", 3) is its own group and survives as that key's floor.
    assert_eq!(seqs(&out, "a"), vec![7, 5, 3]);
    assert!(!out.contains_key(&(0, b"a".to_vec(), Reverse(1))));
    assert!(out.contains_key(&(1, b"a".to_vec(), Reverse(3))));
}

#[test]
fn a_failing_sweep_ends_the_merge_with_its_error() {
    let good = merged(&[(0, "a", 1, put("x")), (0, "z", 2, put("y"))]);
    let bad: Vec<Result<(MemKey, Op)>> = vec![
        Ok(((0, b"m".to_vec(), Reverse(3)), put("m"))),
        Err(Error::Corrupt("boom".into())),
    ];
    let out: Vec<Result<(MemKey, Op)>> = MergeIter::new(vec![
        good.into_iter().map(Ok).collect::<Vec<_>>().into_iter(),
        bad.into_iter(),
    ])
    .collect();
    let errs = out.iter().filter(|r| r.is_err()).count();
    assert_eq!(errs, 1, "exactly one error, then the merge ends");
    assert!(matches!(out.last(), Some(Err(Error::Corrupt(_)))));
    // Through the GC and into the writer, that error aborts the file.
    let dir = std::env::temp_dir().join(format!("drs-merge-error-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let r = sst::write_sorted(
        &dir.join("sst-000001"),
        gc_versions(
            vec![Err::<(MemKey, Op), _>(Error::Corrupt("boom".into()))].into_iter(),
            0,
        ),
        1,
        1,
    );
    assert!(matches!(r, Err(Error::Corrupt(_))));
    let _ = std::fs::remove_dir_all(&dir);
}
