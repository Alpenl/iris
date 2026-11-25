//! Bounded JSON validation and serialization shared by host stores and protocol-2 runtime.
//! Validates nesting depth, byte size, finite floats, and duplicate keys before decoding.

use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeSet;
use std::fmt;

pub const JSON_DEPTH_LIMIT: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundedJsonError {
    QuotaExceeded,
    Invalid,
}

impl fmt::Display for BoundedJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuotaExceeded => f.write_str("JSON size limit exceeded"),
            Self::Invalid => f.write_str("invalid or malformed JSON"),
        }
    }
}

impl std::error::Error for BoundedJsonError {}

struct Bounded(usize);

impl<'de> DeserializeSeed<'de> for Bounded {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        if self.0 > JSON_DEPTH_LIMIT {
            return Err(de::Error::custom("JSON nesting limit"));
        }
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Bounded {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<(), E> {
        if v.is_finite() {
            Ok(())
        } else {
            Err(E::custom("non-finite value"))
        }
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        if self.0 >= JSON_DEPTH_LIMIT {
            return Err(de::Error::custom("JSON nesting limit"));
        }
        while seq.next_element_seed(Bounded(self.0 + 1))?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        if self.0 >= JSON_DEPTH_LIMIT {
            return Err(de::Error::custom("JSON nesting limit"));
        }
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate key"));
            }
            map.next_value_seed(Bounded(self.0 + 1))?;
        }
        Ok(())
    }
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> Result<T, BoundedJsonError> {
    if bytes.len() > limit {
        return Err(BoundedJsonError::QuotaExceeded);
    }
    let mut d = serde_json::Deserializer::from_slice(bytes);
    Bounded(0)
        .deserialize(&mut d)
        .map_err(|_| BoundedJsonError::Invalid)?;
    d.end().map_err(|_| BoundedJsonError::Invalid)?;
    T::deserialize(&mut serde_json::Deserializer::from_slice(bytes))
        .map_err(|_| BoundedJsonError::Invalid)
}

/// Bounds the serialized representation, including escaping, before allocating it.
pub fn encode<T: serde::Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, BoundedJsonError> {
    struct Writer {
        bytes: Vec<u8>,
        limit: usize,
        exceeded: bool,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(std::io::ErrorKind::OutOfMemory.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::new(),
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return Err(if writer.exceeded {
            BoundedJsonError::QuotaExceeded
        } else {
            BoundedJsonError::Invalid
        });
    }
    Ok(writer.bytes)
}

/// Also rejects fields ignored by nested serde types. Missing defaulted
/// fields remain legal; arbitrary map keys are preserved by their map schema.
pub fn decode_exact<T: DeserializeOwned + serde::Serialize>(
    bytes: &[u8],
    limit: usize,
) -> Result<T, BoundedJsonError> {
    let wire: serde_json::Value = decode(bytes, limit)?;
    let value: T = serde_json::from_slice(bytes).map_err(|_| BoundedJsonError::Invalid)?;
    let normalized = serde_json::to_value(&value).map_err(|_| BoundedJsonError::Invalid)?;
    fn known(input: &serde_json::Value, normalized: &serde_json::Value) -> bool {
        match (input, normalized) {
            (serde_json::Value::Object(a), serde_json::Value::Object(b)) => a
                .iter()
                .all(|(key, value)| b.get(key).is_some_and(|other| known(value, other))),
            (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| known(a, b))
            }
            _ => true,
        }
    }
    if known(&wire, &normalized) {
        Ok(value)
    } else {
        Err(BoundedJsonError::Invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_keys_depth_and_escaped_overflow() {
        assert_eq!(
            decode::<serde_json::Value>(br#"{"x":{"a":1,"a":2}}"#, 65536),
            Err(BoundedJsonError::Invalid)
        );
        let deep = format!("{}0{}", "[".repeat(33), "]".repeat(33));
        assert_eq!(
            decode::<serde_json::Value>(deep.as_bytes(), 65536),
            Err(BoundedJsonError::Invalid)
        );
        assert_eq!(encode(&"\0\0", 13), Err(BoundedJsonError::QuotaExceeded));
        assert_eq!(encode(&"\0\0", 14).unwrap(), br#""\u0000\u0000""#);
    }
}
