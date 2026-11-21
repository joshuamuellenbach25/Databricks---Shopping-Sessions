use std::cmp::Ordering;
use std::sync::{Mutex, OnceLock};

use hashbrown::HashMap;

/// An ID as a fixed 16-byte key. IDs of at most 15 bytes are stored inline,
/// big-endian with the length in the low byte, so the derived ordering equals
/// byte-wise ordering. Longer IDs are interned and compared through the table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub u128);

const SHORT_MAX: usize = 15;
const LONG_MARK: u128 = 0xFF;

#[derive(Default)]
struct LongIds {
    index: HashMap<Vec<u8>, u32>,
    bytes: Vec<Vec<u8>>,
}

fn long_ids() -> &'static Mutex<LongIds> {
    static LONG: OnceLock<Mutex<LongIds>> = OnceLock::new();
    LONG.get_or_init(|| Mutex::new(LongIds::default()))
}

impl Key {
    pub fn from_bytes(b: &[u8]) -> Key {
        if b.len() <= SHORT_MAX {
            let mut buf = [0u8; 16];
            buf[..b.len()].copy_from_slice(b);
            buf[15] = b.len() as u8;
            Key(u128::from_be_bytes(buf))
        } else {
            let mut ids = long_ids().lock().unwrap();
            let next = ids.bytes.len() as u32;
            let index = *ids.index.entry(b.to_vec()).or_insert(next);
            if index == next {
                ids.bytes.push(b.to_vec());
            }
            Key(((index as u128) << 8) | LONG_MARK)
        }
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn is_short(self) -> bool {
        (self.0 & 0xFF) != LONG_MARK
    }

    pub fn to_vec(self) -> Vec<u8> {
        if self.is_short() {
            let buf = self.0.to_be_bytes();
            let len = buf[15] as usize;
            buf[..len].to_vec()
        } else {
            let ids = long_ids().lock().unwrap();
            ids.bytes[(self.0 >> 8) as usize].clone()
        }
    }

    pub fn cmp_bytes(self, other: Key) -> Ordering {
        if self.is_short() && other.is_short() {
            self.0.cmp(&other.0)
        } else {
            self.to_vec().cmp(&other.to_vec())
        }
    }

    /// Spreads the key over `1 << bits` buckets.
    pub fn bucket(self, bits: u32) -> usize {
        let x = (self.0 as u64) ^ ((self.0 >> 64) as u64);
        (x.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_orders_like_bytes() {
        let samples: Vec<&[u8]> = vec![
            b"", b"u1", b"U1", b" u1", b"u1 ", b"u10", b"u2", b"e3\r", b"abcdefghijklmno",
            b"abcdefghijklmnop", b"abcdefghijklmnopq", b"user_id_a\tuser_id_b",
        ];
        for a in &samples {
            assert_eq!(Key::from_bytes(a).to_vec(), a.to_vec());
            for b in &samples {
                assert_eq!(Key::from_bytes(a).cmp_bytes(Key::from_bytes(b)), a.cmp(b), "{a:?} vs {b:?}");
                assert_eq!(Key::from_bytes(a) == Key::from_bytes(b), a == b);
            }
        }
        assert!(Key::from_bytes(b"").is_empty());
        assert!(!Key::from_bytes(b"0").is_empty());
    }
}
