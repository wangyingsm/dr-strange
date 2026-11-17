use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::*;

/// A one-request HTTP server answering `(status, body)`, which hands back its
/// base URL and what it was actually sent — head and body — so a test can
/// assert on the request rather than on what this module believes it builds.
fn server(status: u16, body: &'static str) -> (String, Arc<Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(String::new()));
    let captured = Arc::clone(&seen);
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut reader = BufReader::new(&stream);
        let mut head = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap_or(0);
            }
            head.push_str(&line);
        }
        let mut payload = vec![0u8; len];
        if len > 0 {
            std::io::Read::read_exact(&mut reader, &mut payload).ok();
        }
        head.push_str(&String::from_utf8_lossy(&payload));
        *captured.lock().unwrap() = head;
        let mut stream = &stream;
        let _ = write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.flush();
    });
    (url, seen)
}

/// Two rows as the search endpoint returns them: the parent column present on
/// one, nanosecond `start_time`, `span_status` as a word.
const HITS: &str = r#"{"took":12,"total":2,"from":0,"size":100,"scan_size":1,"hits":[
  {"trace_id":"5b8efff798038103d269b633813fc60c","span_id":"eee19b7ec3c1b174",
   "service_name":"gateway","span_status":"UNSET","start_time":1763366400000000000},
  {"trace_id":"5b8efff798038103d269b633813fc60c","span_id":"f0e1d2c3b4a59687",
   "reference_parent_span_id":"eee19b7ec3c1b174","service_name":"checkout",
   "span_status":"ERROR","start_time":"1763366400010000000"}
]}"#;

fn query() -> Query {
    Query {
        base_url: "https://observe.internal:5080".into(),
        org: "default".into(),
        stream: "default".into(),
        start_us: 1_763_366_400_000_000,
        end_us: 1_763_366_460_000_000,
        limit: 100,
    }
}

#[test]
fn a_window_is_one_count_and_one_unit() {
    assert_eq!(window("30s").unwrap(), Duration::from_secs(30));
    assert_eq!(window("15m").unwrap(), Duration::from_secs(900));
    assert_eq!(window("6h").unwrap(), Duration::from_secs(21_600));
    assert_eq!(window("7d").unwrap(), Duration::from_secs(604_800));
}

#[test]
fn a_window_that_states_no_interval_says_what_to_write_instead() {
    for spec in ["1", "", "h", "6w", "1h30m", "0h", "-1h"] {
        let err = window(spec).unwrap_err().to_string();
        assert!(!err.is_empty(), "{spec} was accepted");
    }
}

#[test]
fn the_query_names_the_columns_the_fold_needs_and_the_stream_it_reads() {
    let body = body(&query());
    let sql = body["query"]["sql"].as_str().unwrap();
    for column in [
        "trace_id",
        "span_id",
        "reference_parent_span_id",
        "service_name",
        "span_status",
        "start_time",
    ] {
        assert!(sql.contains(column), "{sql} is missing {column}");
    }
    assert!(sql.contains(r#"FROM "default""#), "{sql}");
    assert!(!sql.contains('*'), "a pull of every attribute: {sql}");
    assert_eq!(body["query"]["start_time"], 1_763_366_400_000_000i64);
    assert_eq!(body["query"]["size"], 100);
}

/// The window is in microseconds and the spans in nanoseconds; a stream name
/// is quoted, so one holding a quote cannot end the identifier early.
#[test]
fn a_stream_name_is_quoted_against_the_identifier_ending_early() {
    let mut q = query();
    q.stream = r#"de"fault"#.into();
    let sql = body(&q)["query"]["sql"].as_str().unwrap().to_string();
    assert!(sql.contains(r#"FROM "de""fault""#), "{sql}");
}

#[test]
fn the_endpoint_names_the_org_and_the_stream_type() {
    assert_eq!(
        url(&query()),
        "https://observe.internal:5080/api/default/_search?type=traces"
    );
    let mut q = query();
    q.base_url = "https://observe.internal:5080/".into();
    assert_eq!(
        url(&q),
        "https://observe.internal:5080/api/default/_search?type=traces"
    );
}

#[test]
fn rows_become_spans_with_the_units_the_fold_expects() {
    let pulled = spans(HITS, 100).unwrap();
    assert_eq!(pulled.rows, 2);
    assert!(!pulled.capped);
    assert!(!pulled.no_parent_column);
    assert_eq!(
        pulled.spans,
        vec![
            Span {
                trace: "5b8efff798038103d269b633813fc60c".into(),
                id: "eee19b7ec3c1b174".into(),
                parent: None,
                service: "gateway".into(),
                start_ms: 1_763_366_400_000,
                error: false,
            },
            Span {
                trace: "5b8efff798038103d269b633813fc60c".into(),
                id: "f0e1d2c3b4a59687".into(),
                parent: Some("eee19b7ec3c1b174".into()),
                service: "checkout".into(),
                start_ms: 1_763_366_400_010,
                error: true,
            },
        ]
    );
}

/// OpenObserve probes its own schema for this column before drawing
/// parent-child edges. A stream without it answers every query and links
/// nothing, which must not read as "these services do not call each other".
#[test]
fn a_stream_whose_rows_carry_no_parent_column_is_reported_not_folded_silently() {
    let text = r#"{"hits":[
      {"span_id":"01","service_name":"a","start_time":1000000},
      {"span_id":"02","service_name":"b","start_time":2000000}]}"#;
    let pulled = spans(text, 100).unwrap();
    assert_eq!(pulled.spans.len(), 2);
    assert!(pulled.no_parent_column);
}

/// A full page means the window held more than was read — and the response's
/// own `total` cannot say so, because a live instance sets it to the row count
/// it returned.
#[test]
fn a_full_page_is_reported_as_a_window_holding_more() {
    let pulled = spans(HITS, 2).unwrap();
    assert!(pulled.capped);
    assert!(!spans(HITS, 3).unwrap().capped);
}

#[test]
fn a_row_with_no_service_or_no_id_is_counted_and_dropped() {
    let text = r#"{"hits":[
      {"span_id":"01","start_time":1000000},
      {"service_name":"b","start_time":1000000},
      {"span_id":"03","service_name":"c","start_time":1000000}]}"#;
    let pulled = spans(text, 100).unwrap();
    assert_eq!(pulled.rows, 3);
    assert_eq!(pulled.spans.len(), 1);
}

#[test]
fn an_answer_that_is_not_a_search_result_is_refused_with_what_came_back() {
    let err = spans(r#"{"code":404,"message":"stream not found"}"#, 100)
        .unwrap_err()
        .to_string();
    assert!(err.contains("hits"), "{err}");
    assert!(err.contains("stream not found"), "{err}");
    assert!(
        spans("<html>", 100)
            .unwrap_err()
            .to_string()
            .contains("not JSON")
    );
}

#[test]
fn a_credential_is_encoded_only_when_it_is_a_pair() {
    assert_eq!(
        authorization("root@example.com:Complexpass#123"),
        "Basic cm9vdEBleGFtcGxlLmNvbTpDb21wbGV4cGFzcyMxMjM="
    );
    assert_eq!(
        authorization("  cm9vdEBleGFtcGxlLmNvbTpwYXNz  "),
        "Basic cm9vdEBleGFtcGxlLmNvbTpwYXNz"
    );
}

/// RFC 4648's own vectors, so the fifteen lines are the standard's base64 and
/// not something that merely round-trips with itself.
#[test]
fn base64_is_rfc_4648() {
    for (plain, encoded) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(plain.as_bytes()), encoded, "{plain}");
    }
    assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
    assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
}

#[test]
fn a_url_with_a_password_in_it_is_not_printed_back() {
    assert_eq!(
        shown("https://root:secret@observe.internal:5080"),
        "https://observe.internal:5080/"
    );
    assert_eq!(
        shown("https://observe.internal:5080"),
        "https://observe.internal:5080"
    );
}

/// The whole request, as a server receives it: the endpoint, the credential,
/// and the SQL — asserted against what was sent rather than what was built.
#[test]
fn a_pull_posts_the_query_with_the_credential_in_a_header() {
    let (url, seen) = server(200, HITS);
    let mut q = query();
    q.base_url = url;
    let pulled = pull(
        &q,
        "probe@example.com:pass",
        &Network::direct(),
        Duration::from_secs(10),
    )
    .unwrap();

    assert_eq!(pulled.spans.len(), 2);
    let sent = seen.lock().unwrap().clone();
    assert!(
        sent.starts_with("POST /api/default/_search?type=traces "),
        "{sent}"
    );
    assert!(
        sent.contains("authorization: Basic cHJvYmVAZXhhbXBsZS5jb206cGFzcw==")
            || sent.contains("Authorization: Basic cHJvYmVAZXhhbXBsZS5jb206cGFzcw=="),
        "{sent}"
    );
    let json: Value = serde_json::from_str(&sent[sent.find('{').expect("a body")..]).unwrap();
    assert_eq!(json["query"]["start_time"], 1_763_366_400_000_000i64);
    assert_eq!(json["query"]["end_time"], 1_763_366_460_000_000i64);
    assert!(
        json["query"]["sql"]
            .as_str()
            .unwrap()
            .contains("reference_parent_span_id"),
        "{sent}"
    );
}

/// The instance's own message is the answer to a refusal: a bare status says
/// nothing about which stream or column it objected to.
#[test]
fn a_refusal_is_reported_with_what_the_instance_said() {
    let (url, _) = server(
        400,
        r#"{"code":20002,"message":"Search stream not found: nosuch"}"#,
    );
    let mut q = query();
    q.base_url = url;
    let err = pull(&q, "t", &Network::direct(), Duration::from_secs(10))
        .unwrap_err()
        .to_string();
    assert!(err.contains("400"), "{err}");
    assert!(err.contains("Search stream not found: nosuch"), "{err}");
}

#[test]
fn an_instance_that_cannot_be_reached_names_itself_without_its_credentials() {
    let q = Query {
        base_url: "http://root:secret@127.0.0.1:1".into(),
        ..query()
    };
    let err = pull(&q, "t", &Network::direct(), Duration::from_millis(500))
        .unwrap_err()
        .to_string();
    assert!(!err.contains("secret"), "{err}");
}
