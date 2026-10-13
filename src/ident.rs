use std::fmt;

const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

pub const DEFAULT_LENGTH: usize = 18;
pub const MIN_LENGTH: usize = 12;
pub const MAX_LENGTH: usize = 24;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AssetId(String);

#[derive(Debug)]
pub enum IdentError {
    LengthOutOfRange,
    Entropy,
}

impl fmt::Display for IdentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthOutOfRange => write!(
                formatter,
                "asset ID length must be between {MIN_LENGTH} and {MAX_LENGTH}"
            ),
            Self::Entropy => formatter.write_str("the operating system random source failed"),
        }
    }
}

impl AssetId {
    pub fn generate(length: usize) -> Result<Self, IdentError> {
        let capacity = capacity(length).ok_or(IdentError::LengthOutOfRange)?;
        let bits = bits_for(capacity);
        let byte_length = bits.div_ceil(8) as usize;
        let mask = if bits == 128 {
            u128::MAX
        } else {
            (1_u128 << bits) - 1
        };

        loop {
            let mut buffer = [0_u8; 16];
            getrandom::fill(&mut buffer[16 - byte_length..]).map_err(|_| IdentError::Entropy)?;
            let value = u128::from_be_bytes(buffer) & mask;
            if value < capacity {
                return Ok(Self(encode(value, length)));
            }
        }
    }

    pub fn parse(value: &str) -> Result<Self, IdentError> {
        if !(MIN_LENGTH..=MAX_LENGTH).contains(&value.len())
            || !value.bytes().all(|byte| ALPHABET.contains(&byte))
        {
            return Err(IdentError::LengthOutOfRange);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn capacity(length: usize) -> Option<u128> {
    let mut capacity: Option<u128> = Some(1);
    for _ in 0..length {
        capacity = capacity?.checked_mul(36);
    }
    capacity
}

fn bits_for(capacity: u128) -> u32 {
    let mut bits = 0_u32;
    while bits < 128 && (1_u128 << bits) < capacity {
        bits += 1;
    }
    bits
}

fn encode(mut value: u128, length: usize) -> String {
    let mut encoded = vec![b'0'; length];
    for position in (0..length).rev() {
        encoded[position] = ALPHABET[(value % 36) as usize];
        value /= 36;
    }
    String::from_utf8(encoded).expect("base36 alphabet is ASCII")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{AssetId, DEFAULT_LENGTH, IdentError, MAX_LENGTH, MIN_LENGTH, capacity, encode};

    #[test]
    fn generated_ids_round_trip() {
        for _ in 0..64 {
            let id = AssetId::generate(DEFAULT_LENGTH).unwrap();
            assert_eq!(id.as_str().len(), DEFAULT_LENGTH);
            assert!(
                id.as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            );
            assert_eq!(AssetId::parse(id.as_str()).unwrap(), id);
        }
    }

    #[test]
    fn ids_use_lowercase_base36_only() {
        let id = AssetId::generate(DEFAULT_LENGTH).unwrap();
        assert!(
            id.as_str()
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn parse_rejects_invalid_ids() {
        assert!(AssetId::parse("short").is_err());
        assert!(AssetId::parse("").is_err());
        assert!(AssetId::parse("AbCdEfGhIjKlMnOpQr").is_err(), "uppercase");
        assert!(AssetId::parse("abcdefghij-lmnopq").is_err(), "hyphen");
        assert!(AssetId::parse("abcdefghij lmnopqr").is_err(), "space");
        assert!(AssetId::parse(&"a".repeat(MAX_LENGTH + 1)).is_err());
    }

    #[test]
    fn generate_respects_configured_length_range() {
        for length in MIN_LENGTH..=MAX_LENGTH {
            let id = AssetId::generate(length).unwrap();
            assert_eq!(id.as_str().len(), length);
            assert_eq!(AssetId::parse(id.as_str()).unwrap(), id);
        }
        assert!(matches!(
            AssetId::generate(MAX_LENGTH + 1),
            Err(IdentError::LengthOutOfRange)
        ));
    }

    #[test]
    fn encode_pads_to_fixed_length() {
        assert_eq!(encode(0, 4), "0000");
        assert_eq!(encode(35, 4), "000z");
        assert_eq!(encode(36, 4), "0010");
    }

    #[test]
    fn capacity_grows_monotonically() {
        let small = capacity(MIN_LENGTH).unwrap();
        let large = capacity(MAX_LENGTH).unwrap();
        assert!(small < large);
    }

    #[test]
    fn ten_thousand_generated_ids_are_unique() {
        let ids: HashSet<_> = (0..10_000)
            .map(|_| AssetId::generate(DEFAULT_LENGTH).unwrap())
            .collect();
        assert_eq!(ids.len(), 10_000);
    }
}
