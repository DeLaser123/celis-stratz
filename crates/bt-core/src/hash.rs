//! Deterministic hashing for experiment identity and golden tests.
//!
//! `canonical_json` relies on `serde_json::Value`'s BTreeMap ordering, so the
//! same logical document always serializes to the same bytes regardless of
//! construction order (spec §3: no hash-map iteration order dependence).

use serde_json::Value;
use sha2::{Digest, Sha256};

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn canonical_json(v: &Value) -> String {
    v.to_string()
}

/// SHA-256 of the canonical JSON serialization.
pub fn hash_json(v: &Value) -> String {
    sha256_hex(canonical_json(v).as_bytes())
}

/// Build a canonical JSON value from key/value pairs in a fixed order.
pub fn json_object(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sha256_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn object_key_order_is_canonical() {
        let a = json_object(&[("b", json!(1)), ("a", json!(2))]);
        let b = json_object(&[("a", json!(2)), ("b", json!(1))]);
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(hash_json(&a), hash_json(&b));
    }
}
