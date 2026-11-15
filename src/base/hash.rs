//! Content addressing: BLAKE3 hashes over a canonical JSON encoding.
//!
//! Everything that is hashed, signed, exported or synchronised goes through [`canonical_json`], so
//! equal values always produce equal bytes regardless of map iteration order or of which serde_json
//! features other crates in the dependency tree happen to enable.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::{Error, Result};

/// A 32-byte BLAKE3 digest. Serialises as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// The all-zero hash, used as the hash of "nothing" (e.g. an empty Merkle tree).
    pub const ZERO: ContentHash = ContentHash([0; 32]);

    /// Hashes raw bytes.
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Hashes the canonical JSON encoding of `value`.
    pub fn of_canonical<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        Ok(Self::of(&canonical_json(value)?))
    }

    /// Hashes the concatenation of several hashes (Merkle interior node).
    pub fn combine<'a>(parts: impl IntoIterator<Item = &'a ContentHash>) -> Self {
        let mut hasher = blake3::Hasher::new();
        for part in parts {
            hasher.update(&part.0);
        }
        Self(*hasher.finalize().as_bytes())
    }

    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Builds a hash from a 32-byte slice (e.g. a SQLite BLOB column).
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let array: [u8; 32] = bytes.try_into().map_err(|_| {
            Error::Invalid(format!(
                "content hash must be 32 bytes, got {}",
                bytes.len()
            ))
        })?;
        Ok(Self(array))
    }

    /// Parses 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut bytes)
            .map_err(|err| Error::Invalid(format!("invalid content hash `{text}`: {err}")))?;
        Ok(Self(bytes))
    }

    /// Raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex, 64 characters.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 12 hex characters; for logs and UIs only.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    /// `true` if this is [`ContentHash::ZERO`].
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 32]
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.short())
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        ContentHash::from_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// Serialises `value` as canonical JSON: object keys sorted bytewise, no insignificant whitespace.
///
/// Non-finite floats are rejected by serde_json before they can reach the output, so the encoding
/// is total over everything Anchor stores.
pub fn canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value)?;
    let mut out = Vec::with_capacity(256);
    write_canonical(&value, &mut out)?;
    Ok(out)
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(number) => out.extend_from_slice(number.to_string().as_bytes()),
        Value::String(text) => serde_json::to_writer(&mut *out, text)?,
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key)?;
                out.push(b':');
                write_canonical(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_sorts_keys_and_strips_whitespace() {
        let a: Value = serde_json::from_str(r#"{ "b": [1, {"z": true, "a": null}], "a": "x" }"#)
            .expect("valid json");
        let b: Value =
            serde_json::from_str(r#"{"a":"x","b":[1,{"a":null,"z":true}]}"#).expect("valid json");
        let bytes = canonical_json(&a).expect("canonical");
        assert_eq!(bytes, canonical_json(&b).expect("canonical"));
        assert_eq!(
            String::from_utf8(bytes).expect("utf8"),
            r#"{"a":"x","b":[1,{"a":null,"z":true}]}"#
        );
    }

    #[test]
    fn hash_hex_round_trip() {
        let hash = ContentHash::of(b"anchor");
        assert_eq!(ContentHash::from_hex(&hash.to_hex()).expect("hex"), hash);
        assert_eq!(
            ContentHash::from_slice(hash.as_bytes()).expect("slice"),
            hash
        );
        assert!(ContentHash::from_hex("abc").is_err());
        assert!(ContentHash::from_slice(&[0u8; 5]).is_err());
        let json = serde_json::to_string(&hash).expect("serialize");
        assert_eq!(
            serde_json::from_str::<ContentHash>(&json).expect("deserialize"),
            hash
        );
        assert!(ContentHash::ZERO.is_zero());
        assert!(!hash.is_zero());
    }

    #[test]
    fn combine_is_order_sensitive() {
        let a = ContentHash::of(b"a");
        let b = ContentHash::of(b"b");
        assert_ne!(
            ContentHash::combine([&a, &b]),
            ContentHash::combine([&b, &a])
        );
    }
}
