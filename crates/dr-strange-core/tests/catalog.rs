//! Soft-schema catalog / introspection (arch/03 §5), through the public API.

use dr_strange_core::{Database, PropDesc, PropValue, Properties, ValueType};

fn props(entries: &[(&str, Option<&str>, PropValue)]) -> Properties {
    entries
        .iter()
        .map(|(k, desc, v)| {
            let pd = match desc {
                Some(d) => PropDesc::described(*d, v.clone()),
                None => PropDesc::new(v.clone()),
            };
            (k.to_string(), pd)
        })
        .collect()
}

#[test]
fn catalog_describes_labels_properties_types_and_descriptions() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();

    // two Persons: one with an int age, one with a float age (soft schema),
    // both describing `name`; one Paper.
    let a = txn
        .create_node(
            &["Person"],
            props(&[
                ("name", Some("full name"), PropValue::Str("Alice".into())),
                ("age", None, PropValue::Int(30)),
            ]),
        )
        .unwrap();
    let b = txn
        .create_node(
            &["Person"],
            props(&[
                ("name", Some("full name"), PropValue::Str("Bob".into())),
                ("age", None, PropValue::Float(41.5)),
            ]),
        )
        .unwrap();
    let paper = txn
        .create_node(&["Paper"], props(&[("year", None, PropValue::Int(2026))]))
        .unwrap();
    txn.create_edge(a, paper, "AUTHORED", Properties::new())
        .unwrap();
    txn.create_edge(b, paper, "AUTHORED", Properties::new())
        .unwrap();
    txn.commit().unwrap();

    let cat = plane.catalog().unwrap();
    assert_eq!(cat.node_count, 3);
    assert_eq!(cat.edge_count, 2);

    let person = &cat.labels["Person"];
    assert_eq!(person.count, 2);
    // name: present on both, always Str, described the same way twice
    let name = &person.properties["name"];
    assert_eq!(name.count, 2);
    assert_eq!(name.types[&ValueType::Str], 2);
    assert_eq!(name.descriptions["full name"], 2);
    // age: int on one, float on the other (observed type frequencies)
    let age = &person.properties["age"];
    assert_eq!(age.count, 2);
    assert_eq!(age.types[&ValueType::Int], 1);
    assert_eq!(age.types[&ValueType::Float], 1);
    assert!(age.descriptions.is_empty());

    assert_eq!(cat.labels["Paper"].count, 1);

    // edge-type connectivity: AUTHORED links Person -> Paper, twice
    let authored = &cat.edge_types["AUTHORED"];
    assert_eq!(authored.count, 2);
    assert_eq!(authored.connection("Person", "Paper"), 2);
}

#[test]
fn catalog_rolls_up_across_planes() {
    let db = Database::in_memory().unwrap();
    for (plane_name, n) in [("startup", 2u64), ("other", 3)] {
        let plane = if plane_name == "startup" {
            db.plane("startup").unwrap()
        } else {
            db.create_plane(plane_name, Properties::new()).unwrap()
        };
        let mut txn = plane.write().unwrap();
        for _ in 0..n {
            txn.create_node(&["Doc"], Properties::new()).unwrap();
        }
        txn.commit().unwrap();
    }

    // per-plane
    assert_eq!(
        db.plane("startup").unwrap().catalog().unwrap().node_count,
        2
    );
    assert_eq!(db.plane("other").unwrap().catalog().unwrap().node_count, 3);
    // rolled up
    let all = db.catalog().unwrap();
    assert_eq!(all.node_count, 5);
    assert_eq!(all.labels["Doc"].count, 5);
}

#[test]
fn catalog_is_empty_for_a_fresh_plane_and_serdes() {
    let db = Database::in_memory().unwrap();
    let cat = db.plane("startup").unwrap().catalog().unwrap();
    assert_eq!(cat.node_count, 0);
    assert!(cat.labels.is_empty());

    // serializable — the MCP layer serves it as JSON (arch/03 §5)
    let json = serde_json::to_string(&cat).unwrap();
    let back: dr_strange_core::CatalogSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(cat, back);
}

#[test]
fn catalog_reflects_deletes() {
    let db = Database::in_memory().unwrap();
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let a = txn.create_node(&["Doc"], Properties::new()).unwrap();
    txn.create_node(&["Doc"], Properties::new()).unwrap();
    txn.commit().unwrap();
    assert_eq!(plane.catalog().unwrap().labels["Doc"].count, 2);

    let mut txn = plane.write().unwrap();
    txn.delete_node(a).unwrap();
    txn.commit().unwrap();
    assert_eq!(plane.catalog().unwrap().labels["Doc"].count, 1);
}
