use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// JSON's portable unsigned 64-bit representation. Handles additionally require
/// 1..=i64::MAX; offsets and lengths may use the entire unsigned range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u64);

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut buf = [0u8; 20];
        let mut value = self.0;
        let mut pos = buf.len();
        loop {
            pos -= 1;
            buf[pos] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let text = std::str::from_utf8(&buf[pos..]).expect("ASCII decimal digits");
        serializer.serialize_str(text)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl de::Visitor<'_> for IdVisitor {
            type Value = Id;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("canonical unsigned decimal string")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                if text.is_empty()
                    || (text.len() > 1 && text.starts_with('0'))
                    || !text.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(E::custom("expected canonical unsigned decimal string"));
                }
                text.parse().map(Id).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(IdVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::Id;

    #[test]
    fn id_zero_alloc_roundtrip_and_strict_validation() {
        for value in [0u64, 1, 9, 10, 42, 9_007_199_254_740_993, u64::MAX] {
            let encoded = serde_json::to_string(&Id(value)).unwrap();
            assert_eq!(encoded, format!("\"{value}\""));
            let decoded: Id = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, Id(value));
        }
        for bad in [
            r#""""#,
            r#""00""#,
            r#""01""#,
            r#""-1""#,
            r#""+1""#,
            r#""12a""#,
            r#""18446744073709551616""#,
            "1",
        ] {
            assert!(serde_json::from_str::<Id>(bad).is_err(), "{bad}");
        }
    }
}
