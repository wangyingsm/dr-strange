use super::*;

/// One checkout span calling one payment span, as the specification writes it:
/// hex ids, integer enums, quoted nanoseconds.
const PAIR: &str = r#"{"resourceSpans":[
  {"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"checkout"}}]},
   "scopeSpans":[{"scope":{"name":"net/http"},"spans":[
     {"traceId":"5B8EFFF798038103D269B633813FC60C","spanId":"EEE19B7EC3C1B174",
      "name":"POST /pay","kind":3,"startTimeUnixNano":"1763366400000000000",
      "endTimeUnixNano":"1763366400120000000"}]}]},
  {"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"payment"}}]},
   "scopeSpans":[{"spans":[
     {"traceId":"5b8efff798038103d269b633813fc60c","spanId":"F0E1D2C3B4A59687",
      "parentSpanId":"eee19b7ec3c1b174","name":"POST /pay","kind":2,
      "startTimeUnixNano":"1763366400010000000","status":{"code":2,"message":"502"}}]}]}
]}"#;

#[test]
fn an_export_states_a_span_per_resource() {
    let read = read(PAIR).unwrap();
    assert_eq!(read.documents, 1);
    assert_eq!(read.anonymous, 0);
    assert_eq!(
        read.spans,
        vec![
            Span {
                trace: "5b8efff798038103d269b633813fc60c".into(),
                id: "eee19b7ec3c1b174".into(),
                parent: None,
                service: "checkout".into(),
                start_ms: 1_763_366_400_000,
                error: false,
            },
            Span {
                trace: "5b8efff798038103d269b633813fc60c".into(),
                id: "f0e1d2c3b4a59687".into(),
                parent: Some("eee19b7ec3c1b174".into()),
                service: "payment".into(),
                start_ms: 1_763_366_400_010,
                error: true,
            },
        ]
    );
}

#[test]
fn ids_are_hex_and_case_is_not_part_of_them() {
    let read = read(PAIR).unwrap();
    assert_eq!(
        read.spans[1].parent.as_deref(),
        Some(read.spans[0].id.as_str())
    );
}

#[test]
fn json_lines_are_a_document_each() {
    let text = format!(
        "{}\n\n{}\n",
        r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"a"}}]},"scopeSpans":[{"spans":[{"spanId":"01","startTimeUnixNano":1000000}]}]}]}"#,
        r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"b"}}]},"scopeSpans":[{"spans":[{"spanId":"02","parentSpanId":"01","startTimeUnixNano":2000000}]}]}]}"#,
    );
    let read = read(&text).unwrap();
    assert_eq!(read.documents, 2);
    assert_eq!(read.spans.len(), 2);
    assert_eq!(read.spans[0].start_ms, 1);
    assert_eq!(read.spans[1].start_ms, 2);
}

#[test]
fn an_enum_written_as_its_name_is_read_though_the_spec_forbids_it() {
    let text = r#"{"resourceSpans":[{"resource":{"attributes":[
        {"key":"service.name","value":{"stringValue":"a"}}]},
        "scopeSpans":[{"spans":[{"spanId":"01","kind":"SPAN_KIND_SERVER",
        "status":{"code":"STATUS_CODE_ERROR"}}]}]}]}"#;
    let read = read(text).unwrap();
    assert!(read.spans[0].error);
}

#[test]
fn a_resource_with_no_service_name_is_counted_not_read() {
    let text = r#"{"resourceSpans":[
        {"resource":{"attributes":[{"key":"host.name","value":{"stringValue":"box"}}]},
         "scopeSpans":[{"spans":[{"spanId":"01"},{"spanId":"02"}]}]},
        {"scopeSpans":[{"spans":[{"spanId":"03"}]}]}]}"#;
    let read = read(text).unwrap();
    assert_eq!(read.anonymous, 3);
    assert!(read.spans.is_empty());
}

#[test]
fn unknown_fields_are_ignored_as_the_spec_requires() {
    let text = r#"{"schemaUrl":"https://x","resourceSpans":[{"resource":{
        "droppedAttributesCount":2,
        "attributes":[{"key":"service.name","value":{"stringValue":"a"}}]},
        "scopeSpans":[{"spans":[{"spanId":"01","links":[],"events":[],"flags":1}]}]}],
        "whatIsThis":{"nested":[1,2,3]}}"#;
    assert_eq!(read(text).unwrap().spans.len(), 1);
}

#[test]
fn an_export_with_no_spans_is_an_empty_read_not_an_error() {
    let read = read(r#"{"resourceSpans":[]}"#).unwrap();
    assert_eq!(read.documents, 1);
    assert!(read.spans.is_empty());
}

#[test]
fn json_that_is_not_an_export_is_refused() {
    for text in [
        "{}",
        r#"{"name":"drsg","version":"2.31.0"}"#,
        r#"{"resourceLogs":[]}"#,
        "",
        "   \n  \n",
    ] {
        let err = read(text).unwrap_err().to_string();
        assert!(
            err.contains("resourceSpans"),
            "{text:?} said {err:?} instead"
        );
    }
}

#[test]
fn a_broken_line_is_named_by_its_number() {
    let good = r#"{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"a"}}]},"scopeSpans":[{"spans":[{"spanId":"01"}]}]}]}"#;
    let text = format!("{good}\n{good}\nnot json at all\n");
    let err = format!("{:#}", read(&text).unwrap_err());
    assert!(err.contains("line 3"), "{err}");
}
