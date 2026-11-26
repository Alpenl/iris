//! Validate before decoding: serde_json alone silently accepts duplicate keys.
use iris_api::v2::{ErrorCode, JSON_DEPTH_LIMIT};
use serde::de::{self, DeserializeOwned, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct BorrowedKey<'de>(Cow<'de, str>);

impl<'de> de::Deserialize<'de> for BorrowedKey<'de> {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = BorrowedKey<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON object key")
            }
            fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(BorrowedKey(Cow::Borrowed(v)))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(BorrowedKey(Cow::Owned(v.to_owned())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(BorrowedKey(Cow::Owned(v)))
            }
        }
        deserializer.deserialize_str(KeyVisitor)
    }
}

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
        while let Some(key) = map.next_key::<BorrowedKey<'de>>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate key"));
            }
            map.next_value_seed(Bounded(self.0 + 1))?;
        }
        Ok(())
    }
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> Result<T, ErrorCode> {
    if bytes.len() > limit {
        return Err(ErrorCode::QuotaExceeded);
    }
    let mut d = serde_json::Deserializer::from_slice(bytes);
    Bounded(0)
        .deserialize(&mut d)
        .map_err(|_| ErrorCode::InvalidArgument)?;
    d.end().map_err(|_| ErrorCode::InvalidArgument)?;
    T::deserialize(&mut serde_json::Deserializer::from_slice(bytes))
        .map_err(|_| ErrorCode::InvalidArgument)
}

/// Bound the serialized representation, including escaping, before allocating it.
pub fn encode<T: serde::Serialize>(value: &T, limit: usize) -> Result<Vec<u8>, ErrorCode> {
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
            ErrorCode::QuotaExceeded
        } else {
            ErrorCode::InvalidArgument
        });
    }
    Ok(writer.bytes)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};
    use iris_api::v2::{ErrorCode, HostCommand, Id};

    #[test]
    fn rejects_duplicate_nested_keys_and_noncanonical_integers() {
        for invalid in [
            r#"{"x":{"a":1,"a":2}}"#,
            r#"{"a":1,"\u0061":2}"#,
            "1e999",
            "{} {}",
        ] {
            assert!(decode::<serde_json::Value>(invalid.as_bytes(), 65536).is_err());
        }
        for invalid in [
            "1",
            "\"01\"",
            "\"+1\"",
            "\"-1\"",
            "\"18446744073709551616\"",
        ] {
            assert!(decode::<Id>(invalid.as_bytes(), 65536).is_err());
        }
        assert_eq!(
            decode::<Id>(b"\"18446744073709551615\"", 65536).unwrap().0,
            u64::MAX
        );
    }

    #[test]
    fn rejects_excess_depth_and_size_before_decode() {
        let deep = format!("{}0{}", "[".repeat(33), "]".repeat(33));
        assert!(decode::<serde_json::Value>(deep.as_bytes(), 65536).is_err());
        assert_eq!(
            decode::<Id>(b"\"1\"", 2).unwrap_err(),
            ErrorCode::QuotaExceeded
        );
    }

    #[test]
    fn encoding_budget_counts_escaped_bytes() {
        assert_eq!(encode(&"\0\0", 13), Err(ErrorCode::QuotaExceeded));
        assert_eq!(encode(&"\0\0", 14).unwrap(), br#""\u0000\u0000""#);
    }

    #[test]
    fn v2_commands_require_decimal_ids_and_reject_unknown_fields() {
        let valid = br#"{"command":"provide_source_page","request_id":"18446744073709551615","page":{"source_id":"s","title":"t","entries":[{"id":"e","name":"n","kind":"video","size":"9007199254740993","modified_secs":"0"}]}}"#;
        let command: HostCommand = decode(valid, 65536).unwrap();
        let wire = encode(&command, 65536).unwrap();
        assert!(
            String::from_utf8(wire)
                .unwrap()
                .contains("\"9007199254740993\"")
        );
        let numeric = String::from_utf8(valid.to_vec())
            .unwrap()
            .replace("\"9007199254740993\"", "9007199254740993");
        assert!(decode::<HostCommand>(numeric.as_bytes(), 65536).is_err());
        let unknown =
            br#"{"command":"source_error","request_id":"1","message":"none","extra":"nope"}"#;
        assert!(decode::<HostCommand>(unknown, 65536).is_err());
        let unknown_nested_variant = br#"{"command":"provide_source_page","request_id":"1","page":{"source_id":"s","folder_id":null,"parent_id":null,"title":"t","entries":[{"id":"e","name":"n","kind":"video","size":null,"modified_secs":null,"duration":null,"subtitle":null,"variants":[{"id":"v1","label":"1080P","extra":1}],"thumbnail":null}],"next_page":null,"auth_hint":null}}"#;
        assert!(decode::<HostCommand>(unknown_nested_variant, 65536).is_err());
        let resolver = std::collections::BTreeSet::from([iris_api::v2::Capability::UrlResolver]);
        assert!(!command.authorized(&resolver));
        let response: HostCommand = decode(
            br#"{"command":"source_error","request_id":"1","message":"none"}"#,
            65536,
        )
        .unwrap();
        assert!(response.authorized(&resolver));
    }
}
