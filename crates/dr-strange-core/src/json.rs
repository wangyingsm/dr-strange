//! JSON ⇄ value conversion for the wrapper layers (arch/05 §2, arch/06) —
//! feature `json`. A friendly dialect for CLI import/export and MCP tool I/O,
//! distinct from postcard (the durable on-disk codec). Plain JSON scalars map
//! to the obvious [`PropValue`]; graph-specific types use escape objects:
//!
//! - `{"$vector": [f32, …]}` → [`PropValue::Vector`] (embeddings)
//! - `{"$desc": "…", "$value": <json>}` → a described property ([`PropDesc`])
//! - `{"$bytes": [u8, …]}` → [`PropValue::Bytes`]
//! - `{"$map": {…}}` → a nested `Map` whose own keys start with `$` (so a
//!   literal `$value` or `$vector` key is data, not an escape)
//!
//! Any other JSON object is a nested `Map`; a plain array is a `List`.
//! Integers must fit `i64` — the only integer [`PropValue`] has — so a
//! larger JSON number is rejected rather than wrapped or rounded.

use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::types::{NodeRecord, PropDesc, PropValue, Properties};

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidArgument(msg.into())
}

// ---- JSON → value --------------------------------------------------------

pub fn json_to_value(v: &Value) -> Result<PropValue> {
    Ok(match v {
        Value::Null => PropValue::Null,
        Value::Bool(b) => PropValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                PropValue::Int(i)
            } else if n.as_u64().is_some() {
                // Above i64::MAX: no PropValue holds it losslessly (Int would
                // wrap negative, Float would round past 2^53), so refuse it.
                return Err(invalid(format!("integer {n} is out of the i64 range")));
            } else {
                PropValue::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => PropValue::Str(s.clone()),
        Value::Array(a) => PropValue::List(a.iter().map(json_to_value).collect::<Result<_>>()?),
        Value::Object(o) => {
            if let Some(vec) = o.get("$vector") {
                PropValue::Vector(json_to_f32_vec(vec)?)
            } else if let Some(bytes) = o.get("$bytes") {
                PropValue::Bytes(json_to_u8_vec(bytes)?)
            } else if let Some(inner) = o.get("$map") {
                let Value::Object(inner) = inner else {
                    return Err(invalid("`$map` must be a JSON object"));
                };
                json_object_to_map(inner)?
            } else {
                json_object_to_map(o)?
            }
        }
    })
}

fn json_object_to_map(o: &serde_json::Map<String, Value>) -> Result<PropValue> {
    let mut map = std::collections::BTreeMap::new();
    for (k, val) in o {
        map.insert(k.clone(), json_to_propdesc(val)?);
    }
    Ok(PropValue::Map(map))
}

/// A property value, optionally wrapped with a `$desc` description.
pub fn json_to_propdesc(v: &Value) -> Result<PropDesc> {
    if let Value::Object(o) = v
        && let Some(value) = o.get("$value")
    {
        let description = o.get("$desc").and_then(|d| d.as_str()).map(str::to_string);
        return Ok(PropDesc {
            description,
            value: json_to_value(value)?,
        });
    }
    Ok(PropDesc::new(json_to_value(v)?))
}

pub fn json_to_properties(v: &Value) -> Result<Properties> {
    let Value::Object(o) = v else {
        return Err(invalid("`properties` must be a JSON object"));
    };
    let mut props = Properties::new();
    for (k, val) in o {
        props.insert(k.clone(), json_to_propdesc(val)?);
    }
    Ok(props)
}

fn json_to_f32_vec(v: &Value) -> Result<Vec<f32>> {
    let Value::Array(a) = v else {
        return Err(invalid("`$vector` must be an array of numbers"));
    };
    a.iter()
        .map(|x| {
            x.as_f64()
                .map(|f| f as f32)
                .ok_or_else(|| invalid("`$vector` element is not a number"))
        })
        .collect()
}

fn json_to_u8_vec(v: &Value) -> Result<Vec<u8>> {
    let Value::Array(a) = v else {
        return Err(invalid("`$bytes` must be an array of byte values"));
    };
    a.iter()
        .map(|x| {
            x.as_u64()
                .filter(|n| *n <= 255)
                .map(|n| n as u8)
                .ok_or_else(|| invalid("`$bytes` element is not a 0..=255 integer"))
        })
        .collect()
}

// ---- value → JSON --------------------------------------------------------

pub fn value_to_json(v: &PropValue) -> Value {
    match v {
        PropValue::Null => Value::Null,
        PropValue::Bool(b) => json!(b),
        PropValue::Int(i) => json!(i),
        PropValue::Float(f) => json!(f),
        PropValue::Str(s) => json!(s),
        PropValue::Bytes(b) => json!({ "$bytes": b }),
        PropValue::Vector(v) => json!({ "$vector": v }),
        PropValue::List(items) => Value::Array(items.iter().map(value_to_json).collect()),
        PropValue::Map(m) => {
            let obj = Value::Object(
                m.iter()
                    .map(|(k, p)| (k.clone(), propdesc_to_json(p)))
                    .collect(),
            );
            // A key starting with `$` would read back as one of the escape
            // objects above (a literal `$value` key becomes a described
            // property), so such a map travels inside its own escape.
            if m.keys().any(|k| k.starts_with('$')) {
                json!({ "$map": obj })
            } else {
                obj
            }
        }
    }
}

/// A projected table as `{"columns": [...], "rows": [[...], ...]}` — one shape
/// for every surface.
///
/// The columns ride with the rows because a table's headers are part of its
/// answer. Rows stay arrays rather than objects: two columns may share a name,
/// and an object would keep one of them.
pub fn table_to_json(table: &crate::Table) -> Value {
    table_with(table, value_to_json)
}

/// [`table_to_json`] with lean values — a projected embedding is its marker.
pub fn table_to_json_lean(table: &crate::Table) -> Value {
    table_with(table, value_to_json_lean)
}

fn table_with(table: &crate::Table, cell: fn(&PropValue) -> Value) -> Value {
    json!({
        "columns": table.columns,
        "rows": table
            .rows
            .iter()
            .map(|row| Value::Array(row.iter().map(cell).collect()))
            .collect::<Vec<_>>(),
    })
}

pub fn propdesc_to_json(p: &PropDesc) -> Value {
    match &p.description {
        Some(desc) => json!({ "$desc": desc, "$value": value_to_json(&p.value) }),
        None => value_to_json(&p.value),
    }
}

pub fn properties_to_json(props: &Properties) -> Value {
    Value::Object(
        props
            .iter()
            .map(|(k, p)| (k.clone(), propdesc_to_json(p)))
            .collect(),
    )
}

/// A full node record as a JSON object (`get`, export, MCP `get_node`).
pub fn node_to_json(node: &NodeRecord) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(node.id.0));
    if let Some(key) = &node.external_key {
        obj.insert("external_key".into(), json!(key));
    }
    obj.insert("labels".into(), json!(node.labels));
    obj.insert("properties".into(), properties_to_json(&node.properties));
    Value::Object(obj)
}

/// [`properties_to_json`] minus vector-valued properties — the lean shape
/// for agent surfaces. An embedding is a thousand floats no reader reads:
/// in a `node.get` reply it is pure context cost (a measured 45 KB for two
/// nodes), so the lean shape replaces each vector with a marker carrying
/// its dimension, keeping the property visible without its payload.
pub fn properties_to_json_lean(props: &Properties) -> Value {
    Value::Object(
        props
            .iter()
            .map(|(k, p)| {
                let v = match &p.value {
                    PropValue::Vector(vec) => vector_marker(vec.len()),
                    _ => propdesc_to_json(p),
                };
                (k.clone(), v)
            })
            .collect(),
    )
}

/// How a vector reads when it is not being sent: how big it is, and that it
/// was left out. One place, so every lean surface says it the same way — a
/// reader (and the dashboard, which turns it into a button) matches on it.
pub fn vector_marker(dims: usize) -> Value {
    json!(format!("$vector({dims} dims, omitted)"))
}

/// [`value_to_json`] with a vector as its marker — for a *projected* column,
/// which is as able to be an embedding as a property is: `RETURN m.embedding`
/// asks for one by name.
pub fn value_to_json_lean(v: &PropValue) -> Value {
    match v {
        PropValue::Vector(vec) => vector_marker(vec.len()),
        PropValue::List(items) => Value::Array(items.iter().map(value_to_json_lean).collect()),
        other => value_to_json(other),
    }
}

/// [`node_to_json`] with lean properties — what RPC's `lean: true` and the
/// MCP tools return.
pub fn node_to_json_lean(node: &NodeRecord) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), json!(node.id.0));
    if let Some(key) = &node.external_key {
        obj.insert("external_key".into(), json!(key));
    }
    obj.insert("labels".into(), json!(node.labels));
    obj.insert(
        "properties".into(),
        properties_to_json_lean(&node.properties),
    );
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lean shape: a vector is a marker with its dimension, everything
    /// else identical — measured 45 KB → ~1 KB per pair of embedded nodes.
    #[test]
    fn lean_replaces_vectors_with_a_marker() {
        let mut props = Properties::new();
        props.insert(
            "embedding".into(),
            PropDesc::new(PropValue::Vector(vec![0.5; 1024])),
        );
        props.insert(
            "doc_comment".into(),
            PropDesc::new(PropValue::Str("real content".into())),
        );
        let lean = properties_to_json_lean(&props);
        assert_eq!(lean["embedding"], json!("$vector(1024 dims, omitted)"));
        assert_eq!(lean["doc_comment"], json!("real content"));
        // The full shape still carries the floats for whoever asks for them.
        let full = properties_to_json(&props);
        assert!(full["embedding"].as_array().is_some() || full["embedding"].is_object());
    }

    /// A column is as able to be an embedding as a property is: `RETURN
    /// m.embedding` asks for one by name, and the table it lands in is read by
    /// the same people who never wanted the floats inline.
    #[test]
    fn a_projected_vector_is_a_marker_too() {
        let table = crate::Table {
            columns: vec!["key".into(), "embedding".into()],
            rows: vec![vec![
                PropValue::Str("m::run".into()),
                PropValue::Vector(vec![0.5; 1024]),
            ]],
        };
        let lean = table_to_json_lean(&table);
        assert_eq!(lean["rows"][0][0], json!("m::run"));
        assert_eq!(lean["rows"][0][1], json!("$vector(1024 dims, omitted)"));

        // And asked for whole, it is still whole.
        let full = table_to_json(&table);
        assert_eq!(
            full["rows"][0][1]["$vector"].as_array().unwrap().len(),
            1024
        );
    }

    #[test]
    fn scalars_round_trip() {
        for v in [
            PropValue::Null,
            PropValue::Bool(true),
            PropValue::Int(-7),
            PropValue::Float(1.5),
            PropValue::Str("hi".into()),
        ] {
            assert_eq!(json_to_value(&value_to_json(&v)).unwrap(), v);
        }
    }

    #[test]
    fn vector_and_bytes_escapes() {
        let vec = PropValue::Vector(vec![0.1, -2.0, 3.5]);
        assert_eq!(json_to_value(&value_to_json(&vec)).unwrap(), vec);
        let bytes = PropValue::Bytes(vec![0, 127, 255]);
        assert_eq!(json_to_value(&value_to_json(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn described_property_round_trips() {
        let p = PropDesc::described("the year", PropValue::Int(2026));
        let j = propdesc_to_json(&p);
        assert_eq!(j, json!({"$desc": "the year", "$value": 2026}));
        assert_eq!(json_to_propdesc(&j).unwrap(), p);
        let bare = PropDesc::new(PropValue::Int(1));
        assert_eq!(propdesc_to_json(&bare), json!(1));
        assert_eq!(json_to_propdesc(&json!(1)).unwrap(), bare);
    }

    #[test]
    fn integer_vs_float_inference() {
        assert_eq!(json_to_value(&json!(30)).unwrap(), PropValue::Int(30));
        assert_eq!(json_to_value(&json!(30.0)).unwrap(), PropValue::Float(30.0));
    }

    #[test]
    fn integers_past_i64_are_rejected_not_wrapped() {
        // Wire-format pin: i64::MAX is the largest integer the dialect
        // accepts; one more is an InvalidArgument, never a negative Int.
        assert_eq!(
            json_to_value(&json!(i64::MAX)).unwrap(),
            PropValue::Int(i64::MAX)
        );
        assert_eq!(
            json_to_value(&json!(i64::MIN)).unwrap(),
            PropValue::Int(i64::MIN)
        );
        let too_big = json!(i64::MAX as u64 + 1);
        assert!(matches!(
            json_to_value(&too_big),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            json_to_value(&json!(u64::MAX)),
            Err(Error::InvalidArgument(_))
        ));
        // Inside a property object the error surfaces the same way.
        assert!(json_to_properties(&json!({"n": u64::MAX})).is_err());
    }

    #[test]
    fn a_map_with_dollar_keys_round_trips_through_its_own_escape() {
        // Wire-format pin: a literal `$value` (or any `$`-key) inside a map
        // is emitted as `{"$map": {...}}`, so reading it back yields the map
        // — not a described property — and the inner keys are untouched.
        let inner: std::collections::BTreeMap<String, PropDesc> = [
            ("$value".to_string(), PropDesc::new(PropValue::Int(1))),
            (
                "$desc".to_string(),
                PropDesc::new(PropValue::Str("d".into())),
            ),
            ("plain".to_string(), PropDesc::new(PropValue::Bool(true))),
        ]
        .into_iter()
        .collect();
        let v = PropValue::Map(inner);
        let j = value_to_json(&v);
        assert_eq!(
            j,
            json!({"$map": {"$value": 1, "$desc": "d", "plain": true}})
        );
        assert_eq!(json_to_value(&j).unwrap(), v);
        // As a described property the escape nests as any other value does.
        let p = PropDesc::described("why", v.clone());
        assert_eq!(json_to_propdesc(&propdesc_to_json(&p)).unwrap(), p);
        // A map without `$` keys is unchanged on the wire.
        let plain = PropValue::Map(
            [("a".to_string(), PropDesc::new(PropValue::Int(1)))]
                .into_iter()
                .collect(),
        );
        assert_eq!(value_to_json(&plain), json!({"a": 1}));
    }

    #[test]
    fn nested_map_of_described_props() {
        let j = json!({
            "name": "Alice",
            "meta": { "score": {"$desc": "0..1", "$value": 0.9} }
        });
        assert_eq!(value_to_json(&json_to_value(&j).unwrap()), j);
    }

    #[test]
    fn properties_object_required() {
        assert!(json_to_properties(&json!([1, 2, 3])).is_err());
        assert_eq!(
            json_to_properties(&json!({"a": 1})).unwrap()["a"].value,
            PropValue::Int(1)
        );
    }
}
