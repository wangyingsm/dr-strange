//! BM25 keyword index (ROADMAP §2) at the `Database` level: declaration,
//! commit-time coherence with node writes, and sidecar survival across reopen.
//! The KV is the source of truth; the `.bm25` sidecar is only a cache, so these
//! assert *correctness* across mutation and reopen, not which load path ran.

use dr_strange_core::{Database, Language, NodeId, PropDesc, PropValue, Properties};
use tempfile::TempDir;

fn body(text: &str) -> Properties {
    [(
        "body".to_string(),
        PropDesc::new(PropValue::Str(text.into())),
    )]
    .into_iter()
    .collect()
}

/// Create three Doc nodes with body text; returns their ids.
fn seed(db: &Database) -> [NodeId; 3] {
    let plane = db.plane("startup").unwrap();
    let mut txn = plane.write().unwrap();
    let a = txn
        .create_node(&["Doc"], body("graph databases store nodes and edges"))
        .unwrap();
    let b = txn
        .create_node(&["Doc"], body("vector search finds similar embeddings"))
        .unwrap();
    let c = txn
        .create_node(
            &["Doc"],
            body("a graph database indexes graph structure for graph queries"),
        )
        .unwrap();
    txn.commit().unwrap();
    [a, b, c]
}

#[test]
fn declared_index_ranks_by_relevance() {
    let db = Database::in_memory().unwrap();
    let [_a, _b, c] = seed(&db);
    let plane = db.plane("startup").unwrap();
    plane
        .ensure_keyword_index("Doc", "body", Language::English)
        .unwrap();

    let hits = plane.keyword_search("Doc", "body", "graph database", 5);
    assert_eq!(hits[0].0, c, "the graph-heavy doc ranks first");
    assert!(hits.iter().all(|(_, s)| *s > 0.0));

    // Undeclared pair ⇒ empty (caller can fall back to a scan).
    assert!(plane.keyword_search("Doc", "missing", "x", 5).is_empty());
}

#[test]
fn writes_after_declaration_stay_coherent() {
    let db = Database::in_memory().unwrap();
    let [a, b, _c] = seed(&db);
    let plane = db.plane("startup").unwrap();
    plane
        .ensure_keyword_index("Doc", "body", Language::English)
        .unwrap();

    // A brand-new node is indexed at commit.
    let plane2 = db.plane("startup").unwrap();
    let d = {
        let mut txn = plane2.write().unwrap();
        let d = txn
            .create_node(&["Doc"], body("graph graph graph everywhere"))
            .unwrap();
        txn.commit().unwrap();
        d
    };
    let hits = plane.keyword_search("Doc", "body", "graph", 5);
    assert_eq!(hits[0].0, d, "the new graph-dense node leads");

    // Re-indexing an existing node's text is reflected.
    {
        let mut txn = plane2.write().unwrap();
        txn.set_prop(
            b,
            "body",
            PropDesc::new(PropValue::Str("nothing relevant".into())),
        )
        .unwrap();
        txn.commit().unwrap();
    }
    assert!(
        plane
            .keyword_search("Doc", "body", "vector embeddings", 5)
            .is_empty(),
        "b no longer matches its old text"
    );

    // Deleting a node drops it from the index.
    {
        let mut txn = plane2.write().unwrap();
        txn.delete_node(a).unwrap();
        txn.commit().unwrap();
    }
    assert!(
        !plane
            .keyword_search("Doc", "body", "graph database", 5)
            .iter()
            .any(|(id, _)| *id == a)
    );
}

/// Chinese end-to-end: jieba segmentation at index AND query time, through
/// declaration, search, relevance ranking, and sidecar reopen. Chinese text
/// has no spaces — without segmentation the split-based analyzer indexes a
/// whole clause as one term and every sub-phrase query misses.
#[test]
fn chinese_index_segments_and_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.drsg");
    let (dense, other);
    {
        let db = Database::open(&path).unwrap();
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        dense = txn
            .create_node(&["Doc"], body("图数据库存储节点与边，图查询遍历图结构"))
            .unwrap();
        other = txn
            .create_node(&["Doc"], body("向量检索按相似度排序结果"))
            .unwrap();
        txn.commit().unwrap();
        plane
            .ensure_keyword_index("Doc", "body", Language::Chinese)
            .unwrap();

        let hits = plane.keyword_search("Doc", "body", "图数据库", 5);
        assert_eq!(hits[0].0, dense, "the graph-dense doc ranks first");
        // A sub-word of the compound must also hit (search-mode granularity).
        assert!(
            plane
                .keyword_search("Doc", "body", "数据库", 5)
                .iter()
                .any(|(id, _)| *id == dense)
        );
        assert!(
            plane
                .keyword_search("Doc", "body", "向量", 5)
                .iter()
                .any(|(id, _)| *id == other)
        );
        // drop → sidecar written
    }
    let db = Database::open(&path).unwrap();
    let plane = db.plane("startup").unwrap();
    let hits = plane.keyword_search("Doc", "body", "图查询", 5);
    assert_eq!(hits[0].0, dense, "segmentation intact after sidecar reload");
}

#[test]
fn index_survives_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.drsg");
    let c;
    {
        let db = Database::open(&path).unwrap();
        [_, _, c] = seed(&db);
        db.plane("startup")
            .unwrap()
            .ensure_keyword_index("Doc", "body", Language::English)
            .unwrap();
        // drop → sidecar written
    }
    let db = Database::open(&path).unwrap();
    let plane = db.plane("startup").unwrap();
    let hits = plane.keyword_search("Doc", "body", "graph database", 5);
    assert_eq!(hits[0].0, c, "ranking intact after reopen");
}
