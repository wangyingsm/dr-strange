//! Record/property codec, v1 (arch/01 §4): postcard.
//!
//! Chosen for M0 because it is actively maintained, compact (varint), and —
//! crucial for durable storage — has a formal wire-format specification
//! independent of the crate version. bincode was rejected (unmaintained,
//! RUSTSEC-2025-0141). The M1 benchmark may still swap the format; that is
//! why everything goes through these functions and nothing else in the
//! crate touches postcard directly. `META_FORMAT_VERSION` gates the format.
//!
//! Postcard is not self-describing: struct field order and enum variant
//! order in `types.rs` (and the raw record structs below) ARE the format.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::types::{NodeId, Properties};

fn corrupt(e: postcard::Error) -> Error {
    Error::Corrupt(format!("record decode failed: {e}"))
}

fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("in-memory serialization cannot fail")
}

/// Decodes exactly `buf` — trailing bytes are corruption, not tolerance.
fn decode<'a, T: Deserialize<'a>>(buf: &'a [u8]) -> Result<T> {
    let (value, rest) = postcard::take_from_bytes::<T>(buf).map_err(corrupt)?;
    if rest.is_empty() {
        Ok(value)
    } else {
        Err(Error::Corrupt(format!(
            "{} trailing bytes after record",
            rest.len()
        )))
    }
}

/// ⚠ On-disk format — field order is the encoding (see module docs).
#[derive(Serialize, Deserialize)]
struct NodeRecordRaw {
    labels: Vec<u32>,
    props: Properties,
}

/// ⚠ On-disk format — field order is the encoding (see module docs).
#[derive(Serialize, Deserialize)]
struct EdgeRecordRaw {
    src: u64,
    dst: u64,
    ty: u32,
    props: Properties,
}

pub fn encode_props(props: &Properties) -> Vec<u8> {
    encode(props)
}

pub fn decode_props(buf: &[u8]) -> Result<Properties> {
    decode(buf)
}

pub fn encode_node_record(labels: &[u32], props: &Properties) -> Vec<u8> {
    encode(&NodeRecordRaw {
        labels: labels.to_vec(),
        props: props.clone(),
    })
}

pub fn decode_node_record(buf: &[u8]) -> Result<(Vec<u32>, Properties)> {
    let raw: NodeRecordRaw = decode(buf)?;
    Ok((raw.labels, raw.props))
}

pub fn encode_edge_record(src: NodeId, dst: NodeId, ty: u32, props: &Properties) -> Vec<u8> {
    encode(&EdgeRecordRaw {
        src: src.0,
        dst: dst.0,
        ty,
        props: props.clone(),
    })
}

pub fn decode_edge_record(buf: &[u8]) -> Result<(NodeId, NodeId, u32, Properties)> {
    let raw: EdgeRecordRaw = decode(buf)?;
    Ok((NodeId(raw.src), NodeId(raw.dst), raw.ty, raw.props))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::types::{PropDesc, PropValue};

    fn sample_props() -> Properties {
        let mut nested = BTreeMap::new();
        nested.insert(
            "inner".to_string(),
            PropDesc::described("nested description", PropValue::Int(-5)),
        );
        let mut p = Properties::new();
        p.insert("name".into(), PropDesc::new(PropValue::Str("Alice".into())));
        p.insert(
            "bio_embedding".into(),
            PropDesc::described(
                "text-embedding of the bio",
                PropValue::Vector(vec![0.25, -1.5, 3.0]),
            ),
        );
        p.insert("active".into(), PropDesc::new(PropValue::Bool(true)));
        p.insert("score".into(), PropDesc::new(PropValue::Float(0.75)));
        p.insert("nothing".into(), PropDesc::new(PropValue::Null));
        p.insert(
            "tags".into(),
            PropDesc::new(PropValue::List(vec![
                PropValue::Str("a".into()),
                PropValue::Int(7),
            ])),
        );
        p.insert(
            "blob".into(),
            PropDesc::new(PropValue::Bytes(vec![1, 2, 3])),
        );
        p.insert("meta".into(), PropDesc::new(PropValue::Map(nested)));
        p
    }

    #[test]
    fn props_roundtrip() {
        let props = sample_props();
        assert_eq!(decode_props(&encode_props(&props)).unwrap(), props);
    }

    #[test]
    fn node_record_roundtrip() {
        let props = sample_props();
        let buf = encode_node_record(&[3, 9], &props);
        let (labels, decoded) = decode_node_record(&buf).unwrap();
        assert_eq!(labels, vec![3, 9]);
        assert_eq!(decoded, props);
    }

    #[test]
    fn edge_record_roundtrip() {
        let props = sample_props();
        let buf = encode_edge_record(NodeId(1), NodeId(2), 5, &props);
        let (src, dst, ty, decoded) = decode_edge_record(&buf).unwrap();
        assert_eq!((src, dst, ty), (NodeId(1), NodeId(2), 5));
        assert_eq!(decoded, props);
    }

    #[test]
    fn absent_description_costs_one_byte() {
        let mut with = Properties::new();
        with.insert("k".into(), PropDesc::described("d", PropValue::Null));
        let mut without = Properties::new();
        without.insert("k".into(), PropDesc::new(PropValue::Null));
        let overhead = encode_props(&with).len() - encode_props(&without).len();
        // Some flag (1) + len varint (1) + "d" (1) vs None flag (1)
        assert_eq!(overhead, 2);
    }

    #[test]
    fn truncated_and_garbage_inputs_error_not_panic() {
        let good = encode_node_record(&[1], &sample_props());
        for cut in 0..good.len() {
            let _ = decode_node_record(&good[..cut]); // must not panic
        }
        assert!(decode_props(&[0xFF, 0xFF]).is_err());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut buf = encode_props(&Properties::new());
        buf.push(0);
        assert!(decode_props(&buf).is_err());
    }

    /// Pins the wire format. If this test fails, the on-disk format changed:
    /// either revert the type change or bump META_FORMAT_VERSION + migrate.
    #[test]
    fn format_pin() {
        assert_eq!(encode_props(&Properties::new()), vec![0]);

        let mut p = Properties::new();
        p.insert("a".into(), PropDesc::new(PropValue::Int(1)));
        assert_eq!(
            encode_props(&p),
            // map len 1 · key len 1 · 'a' · description None · variant Int(2) · zigzag(1)
            vec![0x01, 0x01, 0x61, 0x00, 0x02, 0x02]
        );
    }
}
