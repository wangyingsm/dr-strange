//! Digest pipeline against the deterministic mock provider (arch/07) — the
//! whole chunk→extract→embed→assemble→apply path, offline.

use anyhow::Result;
use dr_strange_core::{Database, Dir, PropValue};
use dr_strange_llm::{CandidateSource, DigestOptions, ExistingEntity, MockProvider, digest};

const REPLY: &str = r#"{
  "entities": [
    {"key":"alice","label":"Person","properties":{"role":"engineer"},"description":"An engineer."},
    {"key":"acme","label":"Company","properties":{},"description":"A robotics company."}
  ],
  "relations": [
    {"src":"alice","dst":"acme","type":"WORKS_AT","description":"Alice works at Acme."},
    {"src":"alice","dst":"ghost","type":"KNOWS"}
  ]
}"#;

fn opts(embed: bool) -> DigestOptions {
    DigestOptions {
        source: "doc.txt".into(),
        model: "mock-1".into(),
        run_id: "run-42".into(),
        chunk_chars: 4000,
        embed,
        concurrency: 1,
        // The mock provider serves one canned reply; the reconciliation pass
        // would consume replies these tests do not provide.
        reconcile: false,
        resolve_identity: false,
        refine: false,
        refine_max_entities: None,
        refine_max_context: None,
    }
}

#[test]
fn digests_entities_relations_provenance_and_embeddings() {
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest(
        "Alice is an engineer at Acme.",
        &mock,
        &mock,
        None,
        &opts(true),
    )
    .unwrap();

    // Two entities; the WORKS_AT relation kept, the dangling KNOWS dropped.
    assert_eq!(result.report.entities, 2);
    assert_eq!(result.report.relations, 1);
    assert_eq!(result.report.dropped_relations, 1);
    assert_eq!(result.report.chat_requests, 1);

    // Nodes carry label, extracted props, a description, provenance, and a vector.
    let alice = result.nodes.iter().find(|n| n.key == "alice").unwrap();
    assert_eq!(alice.label, "Person");
    assert!(matches!(&alice.props["role"].value, PropValue::Str(s) if s == "engineer"));
    assert!(alice.props.contains_key("description"));
    assert!(matches!(&alice.props["embedding"].value, PropValue::Vector(v) if v.len() == 8));
    for key in ["_source", "_model", "_run"] {
        assert!(alice.props.contains_key(key), "missing provenance {key}");
    }
    assert!(matches!(&alice.props["_run"].value, PropValue::Str(s) if s == "run-42"));

    // The kept edge references entity keys.
    let e = &result.edges[0];
    assert_eq!(
        (e.src.as_str(), e.ty.as_str(), e.dst.as_str()),
        ("alice", "WORKS_AT", "acme")
    );
}

#[test]
fn apply_writes_the_graph() {
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest("Alice at Acme.", &mock, &mock, None, &opts(false)).unwrap();

    let db = Database::in_memory().unwrap();
    {
        let plane = db.plane("startup").unwrap();
        let mut txn = plane.write().unwrap();
        let stats = result.apply(&mut txn).unwrap();
        assert_eq!(stats.nodes, 2);
        assert_eq!(stats.edges, 1);
        txn.commit().unwrap();
    }

    let plane = db.plane("startup").unwrap();
    let alice = plane.node_by_key("alice").unwrap().unwrap();
    let acme = plane.node_by_key("acme").unwrap().unwrap();
    let hops = plane.neighbors(alice.id, Dir::Out, None).unwrap();
    assert_eq!(hops.len(), 1);
    assert_eq!(hops[0].node, acme.id);
}

#[test]
fn no_embed_leaves_no_vectors() {
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest("Alice at Acme.", &mock, &mock, None, &opts(false)).unwrap();
    assert!(
        result
            .nodes
            .iter()
            .all(|n| !n.props.contains_key("embedding"))
    );
}

/// A candidate source that always surfaces `acme` as an existing entity.
struct AcmeExists;
impl CandidateSource for AcmeExists {
    fn similar(&self, _query: &[f32], _k: usize) -> Result<Vec<ExistingEntity>> {
        Ok(vec![ExistingEntity {
            key: "acme".into(),
            label: "Company".into(),
            description: "a robotics company".into(),
        }])
    }
}

#[test]
fn linking_dedups_existing_entity_but_keeps_its_edge() {
    // The model still emits both entities + the WORKS_AT edge (REPLY), but the
    // candidate source says `acme` already exists — so acme is linked, not
    // re-created, while the edge to it survives for the bulk loader to resolve.
    let mock = MockProvider::new(vec![REPLY.to_string()], 8);
    let result = digest(
        "Alice is an engineer at Acme.",
        &mock,
        &mock,
        Some(&AcmeExists),
        &opts(false),
    )
    .unwrap();

    // acme dropped from new nodes (it exists); alice remains new.
    let keys: Vec<&str> = result.nodes.iter().map(|n| n.key.as_str()).collect();
    assert_eq!(keys, ["alice"]);
    assert_eq!(result.report.entities, 1);
    assert_eq!(result.report.linked, 1);

    // The alice→acme edge is kept even though acme isn't a fresh node.
    assert_eq!(result.report.relations, 1);
    let e = &result.edges[0];
    assert_eq!(
        (e.src.as_str(), e.ty.as_str(), e.dst.as_str()),
        ("alice", "WORKS_AT", "acme")
    );
}

#[test]
fn re_splits_a_chunk_that_overflows_the_output_limit() {
    use dr_strange_llm::{Chat, ChatReply, OutputTruncated};
    use std::sync::Mutex;

    // A chat that truncates on any prompt with more than `limit_words` words,
    // and otherwise returns one entity keyed by the prompt's first word.
    struct DenseChat {
        limit_words: usize,
        calls: Mutex<usize>,
    }
    impl Chat for DenseChat {
        fn complete(&self, _system: &str, user: &str) -> Result<ChatReply> {
            *self.calls.lock().unwrap() += 1;
            if user.split_whitespace().count() > self.limit_words {
                return Err(anyhow::Error::new(OutputTruncated { limit: 8192 }));
            }
            let first = user.split_whitespace().next().unwrap_or("x");
            Ok(ChatReply {
                input_tokens: 1,
                output_tokens: 1,
                text: format!(
                    r#"{{"entities":[{{"key":"{first}","label":"Chunk","properties":{{}}}}],"relations":[]}}"#
                ),
            })
        }
    }

    // 100 distinct words (~700 chars) form one chunk that overflows the output
    // limit; splitting halves it into pieces small enough to extract.
    let document: String = (0..100).map(|i| format!("w{i:02} ")).collect();
    let dense = DenseChat {
        limit_words: 60,
        calls: Mutex::new(0),
    };
    let mock = MockProvider::new(vec![], 8);

    let result = digest(&document, &dense, &mock, None, &opts(false)).unwrap();

    // The single dense chunk was split and every piece extracted.
    assert!(result.nodes.len() >= 2, "chunk should have been re-split");
    assert!(
        *dense.calls.lock().unwrap() >= 3,
        "one truncated call plus one per piece"
    );
}

// ---- vocabulary reconciliation (ROADMAP §8 stage 1) -----------------------

/// Two chunks reading the same material independently, exactly as the real
/// pipeline produces: the same entity kind spelled two ways, and one
/// relationship named two ways. Chunk order is deterministic, so reply order is.
const CHUNK_A: &str = r#"{
  "entities": [
    {"key":"transformer","label":"Model Architecture","description":"The Transformer."},
    {"key":"bytenet","label":"Model Architecture","description":"ByteNet."}
  ],
  "relations": [
    {"src":"transformer","dst":"bytenet","type":"COMPARED_WITH","description":"Compared."}
  ]
}"#;
const CHUNK_B: &str = r#"{
  "entities": [
    {"key":"convs2s","label":"ModelArchitecture","description":"ConvS2S."}
  ],
  "relations": [
    {"src":"transformer","dst":"convs2s","type":"CONTRASTS_WITH","description":"Contrasted."}
  ]
}"#;
/// The label vocabulary needs no model at all here — folding collapses the two
/// spellings to one name, and one name is canonical by definition — so only the
/// edge types, which no string rule can equate, reach the model.
const TYPE_MERGE: &str = r#"{"groups":[["COMPARED_WITH","CONTRASTS_WITH"]]}"#;

/// Two paragraphs that cannot share a chunk: `chunk` floors the target size at
/// 200 characters, so each paragraph is 150 and the pair overruns it.
fn two_chunk_doc() -> String {
    format!("{}\n\n{}", "a".repeat(150), "b".repeat(150))
}

#[test]
fn reconciliation_folds_spellings_and_merges_synonyms() -> Result<()> {
    let chat = MockProvider::new(vec![CHUNK_A.into(), CHUNK_B.into(), TYPE_MERGE.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200; // the floor; two 150-char paragraphs cannot share one
    o.reconcile = true;
    o.concurrency = 1; // keep reply order aligned with chunk order
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &o)?;

    // One label survives: the spelling variant folded, model-free.
    let labels: Vec<&str> = res.nodes.iter().map(|n| n.label.as_str()).collect();
    assert!(
        labels.iter().all(|l| *l == "Model Architecture"),
        "labels not reconciled: {labels:?}"
    );
    assert_eq!(res.report.labels.before, 2);
    assert_eq!(res.report.labels.after, 1);
    assert_eq!(res.report.labels.folded, 1);
    assert_eq!(
        res.report.labels.chat_requests, 0,
        "folding settled the labels, so the model was never asked"
    );

    // One edge type survives, this time on the model's judgement.
    let types: Vec<&str> = res.edges.iter().map(|e| e.ty.as_str()).collect();
    assert!(
        types.iter().all(|t| *t == "COMPARED_WITH"),
        "edge types not reconciled: {types:?}"
    );
    assert_eq!(res.report.edge_types.merged, 1);
    assert_eq!(res.report.edge_types.after, 1);
    Ok(())
}

#[test]
fn the_wording_the_document_used_survives_as_an_alias() -> Result<()> {
    let chat = MockProvider::new(vec![CHUNK_A.into(), CHUNK_B.into(), TYPE_MERGE.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.reconcile = true;
    o.concurrency = 1;
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &o)?;

    // The renamed node records what the document wrote; untouched ones don't.
    let renamed = res.nodes.iter().find(|n| n.key == "convs2s").unwrap();
    assert_eq!(
        renamed.props.get("_label_as_written").map(|p| &p.value),
        Some(&PropValue::Str("ModelArchitecture".into()))
    );
    let untouched = res.nodes.iter().find(|n| n.key == "bytenet").unwrap();
    assert!(!untouched.props.contains_key("_label_as_written"));

    let renamed = res
        .edges
        .iter()
        .find(|e| e.dst == "convs2s")
        .expect("the contrasted edge");
    assert_eq!(
        renamed.props.get("_type_as_written").map(|p| &p.value),
        Some(&PropValue::Str("CONTRASTS_WITH".into()))
    );
    Ok(())
}

#[test]
fn reconciliation_is_off_by_request_and_costs_two_calls_when_on() -> Result<()> {
    let replies: Vec<String> = vec![CHUNK_A.into(), CHUNK_B.into(), TYPE_MERGE.into()];
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;

    o.reconcile = false;
    let chat = MockProvider::new(replies.clone(), 4);
    let off = digest(&two_chunk_doc(), &chat, &chat, None, &o)?;
    assert_eq!(off.report.chat_requests, 2, "one call per chunk");
    assert_eq!(off.report.labels, Default::default());
    // Without the pass both spellings survive — the state ROADMAP §8 describes.
    let mut labels: Vec<&str> = off.nodes.iter().map(|n| n.label.as_str()).collect();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels, vec!["Model Architecture", "ModelArchitecture"]);

    o.reconcile = true;
    let chat = MockProvider::new(replies, 4);
    let on = digest(&two_chunk_doc(), &chat, &chat, None, &o)?;
    // One call for the edge types; none for the labels, which folding settled.
    assert_eq!(
        on.report.chat_requests, 3,
        "at most +1 per vocabulary, O(1) in doc size"
    );
    assert_eq!(on.report.edge_types.chat_requests, 1);
    Ok(())
}

#[test]
fn renaming_collapses_relations_that_became_the_same_triple() -> Result<()> {
    // Both chunks state the same pair, under two names for one relationship.
    const A: &str = r#"{"entities":[{"key":"x","label":"L"},{"key":"y","label":"L"}],
        "relations":[{"src":"x","dst":"y","type":"COMPARED_WITH"}]}"#;
    const B: &str = r#"{"entities":[{"key":"x","label":"L"}],
        "relations":[{"src":"x","dst":"y","type":"CONTRASTS_WITH"}]}"#;
    let chat = MockProvider::new(vec![A.into(), B.into(), TYPE_MERGE.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.reconcile = true;
    o.concurrency = 1;
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &o)?;
    assert_eq!(res.edges.len(), 1, "the duplicate triple collapsed");
    assert_eq!(res.report.merged_relations, 1);
    Ok(())
}

// ---- identity resolution (ROADMAP §8 stage 2) -----------------------------

/// One chunk naming an entity two ways, the other naming an abbreviation and a
/// variant — the three shapes stage 2 has to tell apart.
const ID_A: &str = r#"{
  "entities": [
    {"key":"Softmax","label":"Function","description":"The softmax function."},
    {"key":"K","label":"Matrix","description":"The key matrix."},
    {"key":"Transformer","label":"Model","description":"The Transformer."}
  ],
  "relations": [{"src":"Transformer","dst":"Softmax","type":"USES"}]
}"#;
const ID_B: &str = r#"{
  "entities": [
    {"key":"softmax","label":"Function","description":"Softmax again."},
    {"key":"Key","label":"Matrix","description":"The key vectors."},
    {"key":"Transformer (big)","label":"Model","description":"The large variant."}
  ],
  "relations": [{"src":"Transformer (big)","dst":"softmax","type":"USES"}]
}"#;
/// `K|Key` is one entity written twice; `Transformer|Transformer (big)` is a
/// model and its variant, so the model leaves that pair out.
const SAME_REPLY: &str = r#"{"same":["K|Key"]}"#;

fn id_opts() -> DigestOptions {
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;
    o.resolve_identity = true;
    o
}

#[test]
fn identity_folds_spellings_merges_aliases_and_keeps_variants() -> Result<()> {
    let chat = MockProvider::new(vec![ID_A.into(), ID_B.into(), SAME_REPLY.into()], 4);
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &id_opts())?;

    let keys: Vec<&str> = res.nodes.iter().map(|n| n.key.as_str()).collect();
    assert!(
        !keys.contains(&"softmax"),
        "the spelling variant folded: {keys:?}"
    );
    assert!(keys.contains(&"Softmax"));
    assert!(
        !keys.contains(&"K"),
        "the abbreviation merged into Key: {keys:?}"
    );
    assert!(keys.contains(&"Key"));
    // A variant is its own entity — merging it would lose the distinction the
    // document drew, and the INSTANCE_OF-style edges that hang off it.
    assert!(keys.contains(&"Transformer"), "{keys:?}");
    assert!(keys.contains(&"Transformer (big)"), "{keys:?}");

    assert_eq!(res.report.identity.folded, 1, "softmax");
    assert_eq!(res.report.identity.merged, 1, "K→Key");
    assert_eq!(res.report.identity.before - res.report.identity.after, 2);
    Ok(())
}

#[test]
fn merging_rewires_edges_onto_the_survivor() -> Result<()> {
    let chat = MockProvider::new(vec![ID_A.into(), ID_B.into(), SAME_REPLY.into()], 4);
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &id_opts())?;

    // Both USES edges pointed at a spelling of softmax; both must now point at
    // the survivor, and the pair collapses to one edge per distinct source.
    for e in &res.edges {
        assert_ne!(e.dst, "softmax", "an edge still points at a folded key");
    }
    let to_softmax: Vec<&str> = res
        .edges
        .iter()
        .filter(|e| e.dst == "Softmax")
        .map(|e| e.src.as_str())
        .collect();
    assert_eq!(to_softmax.len(), 2, "one from each model: {to_softmax:?}");
    Ok(())
}

#[test]
fn an_absorbed_key_survives_as_an_alias() -> Result<()> {
    let chat = MockProvider::new(vec![ID_A.into(), ID_B.into(), SAME_REPLY.into()], 4);
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &id_opts())?;

    let key_node = res.nodes.iter().find(|n| n.key == "Key").unwrap();
    assert_eq!(
        key_node.props.get("_key_as_written").map(|p| &p.value),
        Some(&PropValue::Str("K".into())),
        "the document's other name for this entity is recoverable"
    );
    // The absorbed entity's own account is kept where the survivor had none.
    assert!(key_node.props.contains_key("description"));
    Ok(())
}

/// A plane that holds `Softmax` already but has no usable embeddings — the
/// state that produced two `ByteNet` nodes under one key (ROADMAP §8).
struct KeyedPlaneNoVectors;
impl CandidateSource for KeyedPlaneNoVectors {
    fn similar(&self, _query: &[f32], _k: usize) -> Result<Vec<ExistingEntity>> {
        Ok(Vec::new()) // every vector empty ⇒ every search empty
    }
    fn existing_keys(&self, keys: &[String]) -> Result<Vec<ExistingEntity>> {
        Ok(keys
            .iter()
            .filter(|k| *k == "Softmax")
            .map(|k| ExistingEntity {
                key: k.clone(),
                label: "Function".into(),
                description: String::new(),
            })
            .collect())
    }
}

#[test]
fn an_exact_key_check_prevents_a_duplicate_when_vectors_cannot() -> Result<()> {
    let chat = MockProvider::new(vec![ID_A.into(), ID_B.into(), SAME_REPLY.into()], 4);
    let res = digest(
        &two_chunk_doc(),
        &chat,
        &chat,
        Some(&KeyedPlaneNoVectors),
        &id_opts(),
    )?;

    // Similarity found nothing, as it must with empty vectors — yet the entity
    // the plane already holds is linked rather than written a second time.
    assert!(
        !res.nodes.iter().any(|n| n.key == "Softmax"),
        "Softmax was re-created under a key the plane already holds"
    );
    assert_eq!(res.report.linked, 1);
    // And its edges survive for the bulk loader to resolve against the plane.
    assert!(res.edges.iter().any(|e| e.dst == "Softmax"));
    Ok(())
}

#[test]
fn identity_resolution_is_off_by_request() -> Result<()> {
    let chat = MockProvider::new(vec![ID_A.into(), ID_B.into(), SAME_REPLY.into()], 4);
    let mut o = id_opts();
    o.resolve_identity = false;
    let res = digest(&two_chunk_doc(), &chat, &chat, None, &o)?;

    let keys: Vec<&str> = res.nodes.iter().map(|n| n.key.as_str()).collect();
    assert!(keys.contains(&"softmax") && keys.contains(&"Softmax"));
    assert!(keys.contains(&"K") && keys.contains(&"Key"));
    assert_eq!(res.report.identity, Default::default());
    Ok(())
}

// ---- per-entity refinement (ROADMAP §8 stage 3) ---------------------------

/// Chunk 0 introduces the Transformer thinly; chunk 1 is where the detail is.
/// One-round extraction keeps chunk 0's account and discards chunk 1's — this
/// is the pass that repairs that.
const REF_A: &str = r#"{
  "entities": [{"key":"Transformer","label":"Model","properties":{"layers":4},
                "description":"A model."}],
  "relations": []
}"#;
const REF_B: &str = r#"{
  "entities": [{"key":"Softmax","label":"Function","description":"Normalizes."}],
  "relations": []
}"#;
/// What the model returns once it can see every passage at once.
const REFINED: &str = r#"{"properties":{"layers":6,"heads":8},
    "description":"An encoder-decoder architecture built on self-attention."}"#;

fn refine_doc() -> String {
    format!(
        "{}\n\n{}",
        format_args!("The Transformer is described here. {}", "x".repeat(120)),
        format_args!(
            "The Transformer has 6 layers and 8 heads. {}",
            "y".repeat(110)
        )
    )
}

#[test]
fn refinement_repairs_what_the_first_chunk_got_wrong() -> Result<()> {
    let chat = MockProvider::new(vec![REF_A.into(), REF_B.into(), REFINED.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;
    o.refine = true;
    let res = digest(&refine_doc(), &chat, &chat, None, &o)?;

    let t = res.nodes.iter().find(|n| n.key == "Transformer").unwrap();
    assert_eq!(
        t.props.get("layers").map(|p| &p.value),
        Some(&PropValue::Int(6)),
        "the fuller reading corrected the first chunk's 4"
    );
    assert_eq!(
        t.props.get("heads").map(|p| &p.value),
        Some(&PropValue::Int(8))
    );
    assert_eq!(
        res.report.refined.props_revised, 2,
        "`layers` corrected and the thin description rewritten"
    );
    assert_eq!(
        res.report.refined.props_added, 1,
        "`heads`, which chunk 0 never saw"
    );
    Ok(())
}

#[test]
fn an_entity_with_nothing_new_to_read_is_never_asked_about() -> Result<()> {
    // Softmax is named in one chunk only, and that is the chunk it came from.
    let chat = MockProvider::new(vec![REF_A.into(), REF_B.into(), REFINED.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;
    o.refine = true;
    let res = digest(&refine_doc(), &chat, &chat, None, &o)?;

    assert_eq!(res.report.refined.eligible, 1, "only the Transformer");
    assert_eq!(
        res.report.refined.chat_requests, 1,
        "one call, not one per entity"
    );
    assert!(res.report.refined.skipped_nothing_new >= 1);
    Ok(())
}

#[test]
fn the_entity_budget_bounds_the_calls() -> Result<()> {
    let chat = MockProvider::new(vec![REF_A.into(), REF_B.into(), REFINED.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;
    o.refine = true;
    o.refine_max_entities = Some(0);
    let res = digest(&refine_doc(), &chat, &chat, None, &o)?;
    assert_eq!(
        res.report.refined.chat_requests, 0,
        "budget of zero asks nothing"
    );
    assert_eq!(
        res.report.refined.eligible, 1,
        "but still reports what it would have"
    );
    Ok(())
}

#[test]
fn refinement_is_off_by_default() -> Result<()> {
    let chat = MockProvider::new(vec![REF_A.into(), REF_B.into()], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;
    let res = digest(&refine_doc(), &chat, &chat, None, &o)?;
    assert_eq!(res.report.refined, Default::default());
    let t = res.nodes.iter().find(|n| n.key == "Transformer").unwrap();
    assert_eq!(
        t.props.get("layers").map(|p| &p.value),
        Some(&PropValue::Int(4))
    );
    Ok(())
}

/// A chat that extracts fine but fails every refinement — the provider blip
/// that killed a live run before refinement was made best-effort.
struct FailsOnRefine {
    inner: MockProvider,
}
impl dr_strange_llm::Chat for FailsOnRefine {
    fn complete(&self, system: &str, user: &str) -> Result<dr_strange_llm::ChatReply> {
        if system.starts_with("You re-read one entity") {
            anyhow::bail!("chat/completions: Connection Failed: connection timed out");
        }
        self.inner.complete(system, user)
    }
}

#[test]
fn a_failed_refinement_costs_that_entity_only() -> Result<()> {
    let chat = FailsOnRefine {
        inner: MockProvider::new(vec![REF_A.into(), REF_B.into()], 4),
    };
    let embed = MockProvider::new(vec![], 4);
    let mut o = opts(false);
    o.chunk_chars = 200;
    o.concurrency = 1;
    o.refine = true;
    let res = digest(&refine_doc(), &chat, &embed, None, &o)?;

    // The digest still produced its graph, and the entity kept what
    // extraction gave it.
    assert_eq!(res.report.refined.failed, 1);
    assert_eq!(res.report.refined.refined, 0);
    let t = res.nodes.iter().find(|n| n.key == "Transformer").unwrap();
    assert_eq!(
        t.props.get("layers").map(|p| &p.value),
        Some(&PropValue::Int(4)),
        "unrefined, not lost"
    );
    Ok(())
}
