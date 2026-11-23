//! Property-based test for the write-time vector coercion (#112), using
//! `proptest`.
//!
//! The three example tests beside the implementation show that promotion
//! happens; what needs proving is that it happens *exactly* where it should.
//! The oracle here is [`expected`] — the rule restated independently of the
//! code that implements it — checked against what the store actually holds
//! over generated values, labels and property names. A failure shrinks to the
//! smallest value that breaks the characterization and is persisted under
//! `proptest-regressions/`.
//!
//! Floats are generated in a modest finite range on purpose: `f64 -> f32`
//! saturates to infinity past `f32::MAX` and NaN is not equal to itself, and
//! neither is a question about *where* coercion applies.
//!
//! Array lengths span the inferred-width floor (16) in both directions, and
//! each case seeds the index at a generated width first, so the two halves of
//! the rule — wide enough to be read as an embedding, or exactly as wide as
//! the index already is — are both exercised.

use dr_strange_core::{Database, IndexWidth, Metric, NodeId, PropDesc, PropValue, Properties};
use proptest::prelude::*;

/// Only `(Doc, emb)` is indexed below; `Note` and `other` are the controls.
const LABELS: &[&str] = &["Doc", "Note"];
const NAMES: &[&str] = &["emb", "other"];

fn number() -> impl Strategy<Value = PropValue> {
    prop_oneof![
        (-1000.0f64..1000.0).prop_map(PropValue::Float),
        (-1000i64..1000).prop_map(PropValue::Int),
    ]
}

/// Values a property can arrive with: numeric arrays (the promotable ones),
/// arrays that are not all numbers, an array that is already a vector, and
/// scalars.
fn value() -> impl Strategy<Value = PropValue> {
    prop_oneof![
        6 => prop::collection::vec(number(), 0..20).prop_map(PropValue::List),
        2 => prop::collection::vec(
                prop_oneof![number(), "[a-z]{0,3}".prop_map(PropValue::Str)],
                1..20,
            )
            .prop_map(PropValue::List),
        1 => prop::collection::vec(-1.0f32..1.0, 1..5).prop_map(PropValue::Vector),
        1 => (-1000i64..1000).prop_map(PropValue::Int),
        1 => "[a-z]{0,4}".prop_map(PropValue::Str),
    ]
}

/// Shortest array read as an embedding on width alone — `MIN_INFERRED_DIM`
/// in `api`, restated here so the oracle does not borrow the implementation's
/// own constant.
const MIN_INFERRED_DIM: usize = 16;

/// The rule, stated without reference to its implementation: under a
/// `(label, property)` some vector index names, a non-empty array of numbers
/// becomes a vector of exactly those numbers as `f32` — if it is at least
/// [`MIN_INFERRED_DIM`] wide, or exactly as wide as that index's own vectors
/// (`seeded`). Every other value — any other label, any other property, an
/// empty array, an array holding anything that is not a number, a short array
/// matching nothing — is stored as it was given.
fn expected(label: &str, name: &str, seeded: usize, value: &PropValue) -> PropValue {
    if !(label == "Doc" && name == "emb") {
        return value.clone();
    }
    let PropValue::List(items) = value else {
        return value.clone();
    };
    if items.is_empty() {
        return value.clone();
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            PropValue::Int(n) => out.push(*n as f32),
            PropValue::Float(f) => out.push(*f as f32),
            _ => return value.clone(),
        }
    }
    if out.len() >= MIN_INFERRED_DIM || out.len() == seeded {
        PropValue::Vector(out)
    } else {
        value.clone()
    }
}

/// Declares the `(Doc, emb)` index and, when `seeded > 0`, gives it one
/// explicit vector of that width — the `{"$vector": [...]}` spelling, which
/// is taken at its word — so the index has a width for the rule to match
/// against. Then writes the node under test and returns what the store holds
/// for `name`.
fn write_and_read(
    label: &str,
    name: &str,
    seeded: usize,
    value: PropValue,
) -> (Database, NodeId, PropValue) {
    let db = Database::in_memory().unwrap();
    let id;
    {
        let plane = db.plane("startup").unwrap();
        plane.ensure_vector_index("Doc", "emb", Metric::L2).unwrap();
        if seeded > 0 {
            let seed: Properties = [(
                "emb".to_string(),
                PropDesc::new(PropValue::Vector(vec![0.125; seeded])),
            )]
            .into_iter()
            .collect();
            let mut w = plane.write().unwrap();
            w.create_node(&["Doc"], seed).unwrap();
            w.commit().unwrap();
        }
        let props: Properties = [(name.to_string(), PropDesc::new(value))]
            .into_iter()
            .collect();
        let mut w = plane.write().unwrap();
        id = w.create_node(&[label], props).unwrap();
        w.commit().unwrap();
    }
    let plane = db.plane("startup").unwrap();
    let stored = plane
        .node(id)
        .unwrap()
        .unwrap()
        .properties
        .get(name)
        .unwrap()
        .value
        .clone();
    (db, id, stored)
}

proptest! {
    /// What is stored is what [`expected`] says, for every generated shape,
    /// against an index that is empty or already a generated width wide.
    #[test]
    fn exactly_the_numeric_arrays_an_index_names_are_promoted(
        label in prop::sample::select(LABELS),
        name in prop::sample::select(NAMES),
        seeded in prop::sample::select(&[0usize, 3, 8, 16, 24][..]),
        v in value(),
    ) {
        let (_db, _id, stored) = write_and_read(label, name, seeded, v.clone());
        prop_assert_eq!(stored, expected(label, name, seeded, &v));
    }

    /// An array at least the floor wide is promoted into an index that was
    /// empty, which then holds exactly that width and answers that vector's
    /// own query — the promotion is what makes the node findable, the point
    /// of doing it at all.
    #[test]
    fn a_promoted_vector_is_indexed_at_its_own_width(
        v in prop::collection::vec(number(), MIN_INFERRED_DIM..24),
    ) {
        let (db, id, stored) = write_and_read("Doc", "emb", 0, PropValue::List(v));
        let PropValue::Vector(vec) = stored else {
            return Err(TestCaseError::fail("a wide numeric array under (Doc, emb) must promote"));
        };
        let plane = db.plane("startup").unwrap();

        let widths = plane.vector_index_widths();
        prop_assert_eq!(widths.len(), 1);
        prop_assert_eq!(widths[0].width, IndexWidth::Uniform(vec.len()));

        let hits = plane
            .query()
            .vector_top_k(Some("Doc"), "emb", vec.clone(), Metric::L2, 1)
            .ids()
            .unwrap();
        prop_assert_eq!(hits, vec![id], "the promoted vector answers its own query");
    }

    /// A short numeric array is promoted exactly when it is the width the
    /// index already holds, and left a list otherwise — the half of the rule
    /// that keeps a handful of scores from sizing an embedding index.
    #[test]
    fn a_short_array_follows_the_index_it_would_land_in(
        seeded in 2usize..MIN_INFERRED_DIM,
        len in 1usize..MIN_INFERRED_DIM,
    ) {
        let v = PropValue::List(vec![PropValue::Float(0.5); len]);
        let (_db, _id, stored) = write_and_read("Doc", "emb", seeded, v);
        prop_assert_eq!(
            matches!(stored, PropValue::Vector(_)),
            len == seeded,
            "promoted iff it matches the index's width"
        );
    }

    /// Coercion is a fixed point: writing back what the store holds changes
    /// nothing. A promoted vector stays a vector, and a value left alone is
    /// not promoted on a second pass through `set_prop`.
    #[test]
    fn writing_back_what_is_stored_is_idempotent(
        label in prop::sample::select(LABELS),
        name in prop::sample::select(NAMES),
        seeded in prop::sample::select(&[0usize, 3, 16][..]),
        v in value(),
    ) {
        let (db, id, stored) = write_and_read(label, name, seeded, v);
        let plane = db.plane("startup").unwrap();
        {
            let mut w = plane.write().unwrap();
            w.set_prop(id, name, PropDesc::new(stored.clone())).unwrap();
            w.commit().unwrap();
        }
        let again = plane
            .node(id)
            .unwrap()
            .unwrap()
            .properties
            .get(name)
            .unwrap()
            .value
            .clone();
        prop_assert_eq!(again, stored);
    }
}
