//! What the digest could and could not read, kept on the plane.
//!
//! Every ingest already produces a [`PreprocessReport`]: which handlers ran and
//! how many facts each contributed, how many files were skipped, which
//! extensions no plugin claimed, and the notes a parser writes to explain a
//! thin graph — *2021 method call(s) left unresolved*, *no installed plugin
//! claims .mod (1)*. All of it went to stdout once and was gone.
//!
//! That left a reader unable to tell the two kinds of silence apart. "Nothing
//! calls this function" and "the file that calls it was never parsed" look
//! identical in a graph, and only the second is a reason to go and look at the
//! source. So the last run's account is written onto the plane beside
//! `synced_commit`, where anything reading the plane can find it, and
//! [`dr_strange_core::compact`] turns the part that changes how a *miss* should
//! be read into a note on the answer itself.
//!
//! Replaced on every digest and fold rather than accumulated: a watched
//! repository folds once per commit, and a history of every one of those in a
//! single property would grow without bound to say something only the newest
//! entry answers.

use dr_strange_core::{Database, PropDesc, PropValue, Result};

use super::{Manifest, PreprocessReport};

/// The plane property this writes. Read by `dr-strange-core`, which cannot
/// call into this crate — the property *is* the interface between them, as
/// `synced_commit` already is.
pub const LEDGER_PROP: &str = "ledger";

fn described(desc: &str, value: PropValue) -> PropDesc {
    PropDesc::described(desc, value)
}

fn list(items: impl IntoIterator<Item = String>) -> PropValue {
    PropValue::List(items.into_iter().map(PropValue::Str).collect())
}

/// Record what this ingest read, onto `plane_name`.
///
/// `plugins` is what ran, not what is installed: a plane's account should stay
/// true after the store has moved on, so the build hash and the artifact it
/// came from are copied in rather than looked up later.
pub fn record_ledger(
    db: &Database,
    plane_name: &str,
    report: &PreprocessReport,
    plugins: &[Manifest],
) -> Result<()> {
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut entries: std::collections::BTreeMap<String, PropDesc> = Default::default();
    entries.insert(
        "at".into(),
        described("when this ingest ran, epoch seconds", PropValue::Int(at)),
    );
    // Only the ones that actually handled something. `plugins` is everything
    // loaded — nine of them on a default install — and listing a parser that
    // touched no file would say this plane rests on work that never happened.
    // The handler list is the record of what ran, and its names are these
    // same marks.
    let ran: Vec<String> = plugins
        .iter()
        .filter(|m| {
            let mark = m.stamp();
            report.handlers.iter().any(|(who, _)| *who == mark)
        })
        .map(|m| match (&m.build, &m.source) {
            (Some(_), Some(source)) => format!("{} ← {source}", m.stamp()),
            _ => m.stamp(),
        })
        .collect();
    if !ran.is_empty() {
        entries.insert(
            "plugins".into(),
            described(
                "the plugin builds that produced these facts, and the artifact each came from — \
                 what turns a node's `_generated_by` hash back into a release",
                list(ran),
            ),
        );
    }
    if !report.handlers.is_empty() {
        entries.insert(
            "handlers".into(),
            described(
                "what each handler contributed",
                list(
                    report
                        .handlers
                        .iter()
                        .map(|(who, facts)| format!("{who} = {facts} fact(s)")),
                ),
            ),
        );
    }
    entries.insert(
        "skipped".into(),
        described(
            "files no handler claimed, that held nothing readable, or that the handler which \
             claimed them could not get through — a miss in this plane may be one of these",
            PropValue::Int(report.skipped as i64),
        ),
    );
    if !report.unclaimed.is_empty() {
        entries.insert(
            "unclaimed".into(),
            described(
                "extensions no installed plugin claims, read as prose instead of parsed",
                list(report.unclaimed.iter().cloned()),
            ),
        );
    }
    if !report.collisions.is_empty() {
        entries.insert(
            "collisions".into(),
            described(
                "keys two handlers both produced — a plugin bug, kept visible",
                list(report.collisions.iter().cloned()),
            ),
        );
    }
    if !report.notes.is_empty() {
        entries.insert(
            "notes".into(),
            described(
                "what the parsers said about their own limits on this tree",
                list(report.notes.iter().cloned()),
            ),
        );
    }

    // One property edited under the write lock, not the map read and written
    // back: `serve watch` stamps the sync point on the same plane, and a
    // read-modify-write here could put the map back without it.
    db.plane(plane_name)?.update_properties(|props| {
        props.insert(
            LEDGER_PROP.into(),
            described(
                "what the ingest that wrote this plane could and could not read",
                PropValue::Map(entries),
            ),
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> PreprocessReport {
        PreprocessReport {
            handlers: vec![("rust@2+5ac6f728".into(), 4211)],
            skipped: 12,
            unclaimed: vec![".mod (1)".into()],
            notes: vec!["2021 method call(s) left unresolved".into()],
            ..Default::default()
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            name: "rust".into(),
            version: "2".into(),
            extensions: vec!["rs".into()],
            logo: None,
            build: Some("5ac6f728".into()),
            source: Some("https://example.invalid/rust-v1.6.0/rust.wasm".into()),
        }
    }

    fn read(db: &Database) -> std::collections::BTreeMap<String, PropDesc> {
        let props = db.plane("code").unwrap().properties().unwrap();
        match &props.get(LEDGER_PROP).unwrap().value {
            PropValue::Map(m) => m.clone(),
            other => panic!("the ledger is not a map: {other:?}"),
        }
    }

    #[test]
    fn the_account_lands_on_the_plane_with_the_build_that_wrote_it() {
        let db = Database::in_memory().unwrap();
        db.create_plane("code", Default::default()).unwrap();
        record_ledger(&db, "code", &report(), &[manifest()]).unwrap();

        let led = read(&db);
        assert_eq!(led["skipped"].value, PropValue::Int(12));
        let plugins = format!("{:?}", led["plugins"].value);
        assert!(plugins.contains("rust@2+5ac6f728"), "{plugins}");
        assert!(
            plugins.contains("rust-v1.6.0"),
            "the artifact is what turns the hash back into a release: {plugins}"
        );
        assert!(format!("{:?}", led["unclaimed"].value).contains(".mod (1)"));
        assert!(format!("{:?}", led["notes"].value).contains("2021 method call"));
    }

    /// A watched repository folds once per commit. The account is the *last*
    /// run's, replaced each time — never a list that grows forever.
    #[test]
    fn a_second_ingest_replaces_the_first() {
        let db = Database::in_memory().unwrap();
        db.create_plane("code", Default::default()).unwrap();
        record_ledger(&db, "code", &report(), &[manifest()]).unwrap();

        let clean = PreprocessReport {
            handlers: vec![("rust@2+5ac6f728".into(), 4300)],
            ..Default::default()
        };
        record_ledger(&db, "code", &clean, &[manifest()]).unwrap();

        let led = read(&db);
        assert_eq!(led["skipped"].value, PropValue::Int(0));
        assert!(!led.contains_key("unclaimed"), "the stale entry is gone");
        assert!(!led.contains_key("notes"));
        assert!(format!("{:?}", led["handlers"].value).contains("4300"));
    }

    /// A default install loads nine plugins and a Rust tree uses two. Listing
    /// the other seven would say this plane rests on work that never ran.
    #[test]
    fn only_the_plugins_that_handled_something_are_named() {
        let db = Database::in_memory().unwrap();
        db.create_plane("code", Default::default()).unwrap();
        let idle = Manifest {
            name: "java".into(),
            version: "1".into(),
            extensions: vec!["java".into()],
            logo: None,
            build: Some("26876c3e".into()),
            source: Some("https://example.invalid/java-v1.2.0/java.wasm".into()),
        };
        record_ledger(&db, "code", &report(), &[manifest(), idle]).unwrap();

        let plugins = format!("{:?}", read(&db)["plugins"].value);
        assert!(plugins.contains("rust@2+5ac6f728"), "{plugins}");
        assert!(
            !plugins.contains("java"),
            "a plugin that handled nothing is not part of this plane's account: {plugins}"
        );
    }

    /// A plane's own properties are not disturbed by writing the account.
    #[test]
    fn the_sync_point_beside_it_survives() {
        let db = Database::in_memory().unwrap();
        db.create_plane("code", Default::default()).unwrap();
        {
            let plane = db.plane("code").unwrap();
            let mut props = plane.properties().unwrap();
            props.insert(
                "synced_commit".into(),
                PropDesc::new(PropValue::Str("abcdef0123456789".into())),
            );
            plane.set_properties(props).unwrap();
        }
        record_ledger(&db, "code", &report(), &[manifest()]).unwrap();

        let props = db.plane("code").unwrap().properties().unwrap();
        assert!(props.contains_key("synced_commit"));
        assert!(props.contains_key(LEDGER_PROP));
    }

    /// The account is written under the plane's write lock, so a property
    /// another writer sets at the same moment is not lost: many threads each
    /// stamping their own property while the ledger is recorded over and
    /// over, and every stamp is still there at the end.
    #[test]
    fn a_property_stamped_while_the_account_is_written_is_kept() {
        let db = Database::in_memory().unwrap();
        db.create_plane("code", Default::default()).unwrap();
        const WRITERS: usize = 8;
        const ROUNDS: usize = 25;
        std::thread::scope(|s| {
            for w in 0..WRITERS {
                let db = &db;
                s.spawn(move || {
                    for r in 0..ROUNDS {
                        let plane = db.plane("code").unwrap();
                        plane
                            .update_properties(|props| {
                                props.insert(
                                    format!("stamp_{w}_{r}"),
                                    PropDesc::new(PropValue::Int(r as i64)),
                                );
                            })
                            .unwrap();
                        record_ledger(db, "code", &report(), &[manifest()]).unwrap();
                    }
                });
            }
        });
        let props = db.plane("code").unwrap().properties().unwrap();
        let stamps = props.keys().filter(|k| k.starts_with("stamp_")).count();
        assert_eq!(stamps, WRITERS * ROUNDS, "every writer's stamp survives");
        assert!(props.contains_key(LEDGER_PROP));
    }
}
