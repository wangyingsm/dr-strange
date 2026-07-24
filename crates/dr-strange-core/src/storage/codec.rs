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
///
/// `external_key` is carried inline (rather than requiring a reverse lookup
/// through `ext_keys`) so `delete_node` can clean up its `ext_keys` entry
/// without an extra index (arch/01 §2).
#[derive(Serialize, Deserialize)]
struct NodeRecordRaw {
    external_key: Option<String>,
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

pub fn encode_node_record(
    external_key: Option<&str>,
    labels: &[u32],
    props: &Properties,
) -> Vec<u8> {
    encode(&NodeRecordRaw {
        external_key: external_key.map(str::to_string),
        labels: labels.to_vec(),
        props: props.clone(),
    })
}

pub fn decode_node_record(buf: &[u8]) -> Result<(Option<String>, Vec<u32>, Properties)> {
    let raw: NodeRecordRaw = decode(buf)?;
    Ok((raw.external_key, raw.labels, raw.props))
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
        let buf = encode_node_record(None, &[3, 9], &props);
        let (key, labels, decoded) = decode_node_record(&buf).unwrap();
        assert_eq!(key, None);
        assert_eq!(labels, vec![3, 9]);
        assert_eq!(decoded, props);
    }

    #[test]
    fn node_record_with_external_key_roundtrips() {
        let props = sample_props();
        let buf = encode_node_record(Some("arxiv:2406.01234"), &[1], &props);
        let (key, labels, decoded) = decode_node_record(&buf).unwrap();
        assert_eq!(key.as_deref(), Some("arxiv:2406.01234"));
        assert_eq!(labels, vec![1]);
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
        let good = encode_node_record(None, &[1], &sample_props());
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

    #[test]
    fn realistic_embedding_roundtrips() {
        // 1536-dim f32 vector — the common text-embedding shape
        let vec: Vec<f32> = (0..1536).map(|i| (i as f32) * 0.001 - 0.75).collect();
        let mut p = Properties::new();
        p.insert(
            "embedding".into(),
            PropDesc::described("model: text-embedding-3-small", PropValue::Vector(vec)),
        );
        assert_eq!(decode_props(&encode_props(&p)).unwrap(), p);
    }

    #[test]
    fn deeply_nested_maps_roundtrip() {
        let mut value = PropValue::Int(0);
        for depth in 0..64 {
            let mut m = BTreeMap::new();
            m.insert(format!("level{depth}"), PropDesc::new(value));
            value = PropValue::Map(m);
        }
        let mut p = Properties::new();
        p.insert("deep".into(), PropDesc::new(value));
        assert_eq!(decode_props(&encode_props(&p)).unwrap(), p);
    }

    #[test]
    fn empty_and_awkward_strings_roundtrip() {
        let mut p = Properties::new();
        p.insert(
            "".into(),
            PropDesc::described("", PropValue::Str("".into())),
        );
        p.insert(
            "emoji-🔑".into(),
            PropDesc::new(PropValue::Str("值\u{0}with\u{0}nulls".into())),
        );
        assert_eq!(decode_props(&encode_props(&p)).unwrap(), p);
    }

    #[test]
    fn extreme_numeric_values_roundtrip() {
        let mut p = Properties::new();
        for (i, v) in [
            PropValue::Int(i64::MIN),
            PropValue::Int(i64::MAX),
            PropValue::Float(f64::MIN_POSITIVE),
            PropValue::Float(f64::MAX),
            PropValue::Float(f64::NEG_INFINITY),
            PropValue::Vector(vec![f32::MIN, f32::MAX, 0.0]),
        ]
        .into_iter()
        .enumerate()
        {
            p.insert(format!("v{i}"), PropDesc::new(v));
        }
        assert_eq!(decode_props(&encode_props(&p)).unwrap(), p);
        // NaN can't be compared with ==; assert it survives as NaN
        let mut nan = Properties::new();
        nan.insert("nan".into(), PropDesc::new(PropValue::Float(f64::NAN)));
        match &decode_props(&encode_props(&nan)).unwrap()["nan"].value {
            PropValue::Float(f) => assert!(f.is_nan()),
            other => panic!("expected float, got {other:?}"),
        }
    }

    /// Deterministic xorshift PRNG — no dependency, reproducible failures.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn string(&mut self) -> String {
            let len = self.below(12);
            (0..len)
                .map(|_| char::from_u32(0x61 + (self.below(26) as u32)).unwrap())
                .collect()
        }

        fn value(&mut self, depth: u32) -> PropValue {
            match self.below(if depth == 0 { 7 } else { 9 }) {
                0 => PropValue::Null,
                1 => PropValue::Bool(self.below(2) == 0),
                2 => PropValue::Int(self.next() as i64),
                3 => PropValue::Float(f64::from_bits(self.next())),
                4 => PropValue::Str(self.string()),
                5 => PropValue::Bytes((0..self.below(20)).map(|_| self.next() as u8).collect()),
                6 => PropValue::Vector(
                    (0..self.below(20))
                        .map(|_| f32::from_bits(self.next() as u32))
                        .collect(),
                ),
                7 => PropValue::List((0..self.below(4)).map(|_| self.value(depth - 1)).collect()),
                _ => PropValue::Map(
                    (0..self.below(4))
                        .map(|_| (self.string(), self.prop_desc(depth - 1)))
                        .collect(),
                ),
            }
        }

        fn prop_desc(&mut self, depth: u32) -> PropDesc {
            PropDesc {
                description: if self.below(3) == 0 {
                    Some(self.string())
                } else {
                    None
                },
                value: self.value(depth),
            }
        }

        fn props(&mut self) -> Properties {
            (0..self.below(8))
                .map(|_| (self.string(), self.prop_desc(3)))
                .collect()
        }
    }

    /// Compares while treating NaN == NaN (bitwise identity is preserved by
    /// the codec; PartialEq is not reflexive for NaN).
    fn eq_props(a: &Properties, b: &Properties) -> bool {
        fn eq_value(a: &PropValue, b: &PropValue) -> bool {
            match (a, b) {
                (PropValue::Float(x), PropValue::Float(y)) => x.to_bits() == y.to_bits(),
                (PropValue::Vector(x), PropValue::Vector(y)) => {
                    x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits())
                }
                (PropValue::List(x), PropValue::List(y)) => {
                    x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq_value(p, q))
                }
                (PropValue::Map(x), PropValue::Map(y)) => eq_map(x, y),
                _ => a == b,
            }
        }
        fn eq_map(a: &BTreeMap<String, PropDesc>, b: &BTreeMap<String, PropDesc>) -> bool {
            a.len() == b.len()
                && a.iter().zip(b).all(|((ka, pa), (kb, pb))| {
                    ka == kb && pa.description == pb.description && eq_value(&pa.value, &pb.value)
                })
        }
        eq_map(a, b)
    }

    #[test]
    fn randomized_roundtrips() {
        let mut rng = Rng(0xD25C_0DE5_EED1_2345);
        for i in 0..500 {
            let props = rng.props();
            let decoded = decode_props(&encode_props(&props))
                .unwrap_or_else(|e| panic!("iteration {i}: decode failed: {e}"));
            assert!(eq_props(&decoded, &props), "iteration {i}: mismatch");
        }
    }

    #[test]
    fn randomized_record_roundtrips() {
        let mut rng = Rng(0xBEEF_CAFE_1234_5678);
        for _ in 0..200 {
            let labels: Vec<u32> = (0..rng.below(5)).map(|_| rng.next() as u32).collect();
            let props = rng.props();
            let key = if rng.below(2) == 0 {
                Some(rng.string())
            } else {
                None
            };
            let (k2, l2, p2) =
                decode_node_record(&encode_node_record(key.as_deref(), &labels, &props)).unwrap();
            assert_eq!(k2, key);
            assert_eq!(l2, labels);
            assert!(eq_props(&p2, &props));

            let (src, dst, ty) = (NodeId(rng.next()), NodeId(rng.next()), rng.next() as u32);
            let (s2, d2, t2, ep) =
                decode_edge_record(&encode_edge_record(src, dst, ty, &props)).unwrap();
            assert_eq!((s2, d2, t2), (src, dst, ty));
            assert!(eq_props(&ep, &props));
        }
    }

    #[test]
    fn randomized_garbage_never_panics() {
        let mut rng = Rng(0x0BAD_F00D_0BAD_F00D);
        for _ in 0..500 {
            let len = rng.below(64) as usize;
            let garbage: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let _ = decode_props(&garbage);
            let _ = decode_node_record(&garbage);
            let _ = decode_edge_record(&garbage);
        }
    }
}
