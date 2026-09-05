//! The query history: what ran, in order, capped.
//!
//! A history is not a log. It is the short list a person picks from and a
//! command re-runs, so it is ordered, bounded, and does not fill with the same
//! line typed twice.

use dr_strange_core::Database;

fn db() -> Database {
    Database::in_memory().unwrap()
}

#[test]
fn a_recorded_query_comes_back_by_id_and_in_the_list() {
    let db = db();
    let id = db
        .record_query("code", "MATCH (n:Fn) RETURN n", Database::DEFAULT_HISTORY)
        .unwrap();

    let one = db
        .recorded_query(id)
        .unwrap()
        .expect("it was just recorded");
    assert_eq!(one.query, "MATCH (n:Fn) RETURN n");
    assert_eq!(one.plane, "code");
    assert_eq!(one.id, id);
    assert!(one.at > 0, "stamped with when it ran");

    let list = db.query_history(10).unwrap();
    assert_eq!(list, vec![one]);
}

#[test]
fn the_newest_is_first() {
    let db = db();
    for q in ["one", "two", "three"] {
        db.record_query("code", q, Database::DEFAULT_HISTORY)
            .unwrap();
    }
    let queries: Vec<String> = db
        .query_history(10)
        .unwrap()
        .into_iter()
        .map(|r| r.query)
        .collect();
    assert_eq!(queries, ["three", "two", "one"]);
}

/// The cap is what makes it a history: the oldest go as new ones arrive.
#[test]
fn the_oldest_are_purged_as_new_ones_arrive() {
    let db = db();
    for i in 0..10 {
        db.record_query("code", &format!("q{i}"), 4).unwrap();
    }
    let list = db.query_history(100).unwrap();
    assert_eq!(list.len(), 4, "four kept, however many ran");
    let queries: Vec<&str> = list.iter().map(|r| r.query.as_str()).collect();
    assert_eq!(queries, ["q9", "q8", "q7", "q6"]);

    // And a purged id is gone rather than wrong.
    let first = list.last().unwrap().id;
    assert!(db.recorded_query(first - 1).unwrap().is_none());
}

/// Running the same thing twice in a row is one entry, restamped — a list you
/// pick from should not be a column of the same line.
#[test]
fn the_same_query_twice_running_is_one_entry() {
    let db = db();
    let first = db.record_query("code", "MATCH (n) RETURN n", 200).unwrap();
    let again = db.record_query("code", "MATCH (n) RETURN n", 200).unwrap();
    assert_eq!(first, again, "the same entry, not a second one");
    assert_eq!(db.query_history(10).unwrap().len(), 1);

    // The same text against another plane is another entry: where it ran is
    // part of what it was.
    db.record_query("docs", "MATCH (n) RETURN n", 200).unwrap();
    assert_eq!(db.query_history(10).unwrap().len(), 2);

    // And coming back to it after something else is a new moment.
    db.record_query("code", "MATCH (m) RETURN m", 200).unwrap();
    db.record_query("code", "MATCH (n) RETURN n", 200).unwrap();
    let queries: Vec<String> = db
        .query_history(10)
        .unwrap()
        .into_iter()
        .map(|r| r.query)
        .collect();
    assert_eq!(queries.len(), 4);
    assert_eq!(queries[0], "MATCH (n) RETURN n");
}

#[test]
fn an_empty_history_is_empty_rather_than_an_error() {
    let db = db();
    assert!(db.query_history(10).unwrap().is_empty());
    assert!(db.recorded_query(1).unwrap().is_none());
}

/// It survives a reopen, like everything else in `meta`.
#[test]
fn history_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("g.drsg");
    {
        let db = Database::open(&path).unwrap();
        db.record_query("code", "MATCH (n:Fn) RETURN n", 200)
            .unwrap();
    }
    let db = Database::open(&path).unwrap();
    let list = db.query_history(10).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].query, "MATCH (n:Fn) RETURN n");
}
