use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

fn serialize_decimal_u64<S: Serializer>(value: u64, serializer: S) -> Result<S::Ok, S::Error> {
    let mut buf = [0u8; 20];
    let mut n = value;
    let mut pos = buf.len();
    loop {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let text = std::str::from_utf8(&buf[pos..]).expect("ASCII decimal digits");
    serializer.serialize_str(text)
}

fn deserialize_decimal_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    struct DecimalVisitor;

    impl de::Visitor<'_> for DecimalVisitor {
        type Value = u64;

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
            text.parse().map_err(E::custom)
        }
    }

    deserializer.deserialize_str(DecimalVisitor)
}

/// Opaque capability handle, request/session identifier, or monotonic revision
/// encoded on the JSON wire as a canonical unsigned decimal string.
/// Host-issued capability handles additionally satisfy `1..=i64::MAX`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u64);

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_decimal_u64(self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_decimal_u64(deserializer).map(Self)
    }
}

/// Portable unsigned 64-bit numeric quantity (byte sizes, offsets, lengths,
/// millisecond timestamps/durations, bitrates, and sequence numbers) encoded on
/// the JSON wire as a canonical unsigned decimal string without IEEE-754 loss.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WireU64(pub u64);

impl From<u64> for WireU64 {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<WireU64> for u64 {
    fn from(value: WireU64) -> Self {
        value.0
    }
}

impl Serialize for WireU64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_decimal_u64(self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for WireU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_decimal_u64(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::{Id, WireU64};

    #[test]
    fn id_and_wire_u64_zero_alloc_roundtrip_and_strict_validation() {
        for value in [0u64, 1, 9, 10, 42, 9_007_199_254_740_993, u64::MAX] {
            let encoded = serde_json::to_string(&Id(value)).unwrap();
            assert_eq!(encoded, format!("\"{value}\""));
            let decoded: Id = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, Id(value));

            let wire = WireU64::from(value);
            assert_eq!(serde_json::to_string(&wire).unwrap(), encoded);
            let decoded_wire: WireU64 = serde_json::from_str(&encoded).unwrap();
            assert_eq!(u64::from(decoded_wire), value);
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
            assert!(serde_json::from_str::<WireU64>(bad).is_err(), "{bad}");
        }
    }
}
