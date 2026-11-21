use std::cmp::Ordering;
use std::sync::{Mutex, OnceLock};

use hashbrown::HashMap;

/// An ID as one 64-bit word. An ID of at most 10 bytes drawn from `0-9`,
/// `A-Z`, `_` and `a-z` packs 6 bits per byte, codes in ASCII order and 0 as
/// padding, so the derived ordering equals byte-wise ordering. Any other ID
/// is interned and carries the top bit; it compares through its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub u64);

const PACKED_MAX: usize = 10;
const LONG: u64 = 1 << 63;

const fn code_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut code = 1u8;
    let mut c = 0usize;
    while c < 256 {
        let b = c as u8;
        if b.is_ascii_digit() || b.is_ascii_uppercase() || b == b'_' || b.is_ascii_lowercase() {
            table[c] = code;
            code += 1;
        }
        c += 1;
    }
    table
}

const fn byte_table() -> [u8; 64] {
    let codes = code_table();
    let mut table = [0u8; 64];
    let mut c = 0usize;
    while c < 256 {
        if codes[c] != 0 {
            table[codes[c] as usize] = c as u8;
        }
        c += 1;
    }
    table
}

static CODE: [u8; 256] = code_table();
static BYTE: [u8; 64] = byte_table();

#[derive(Default)]
struct LongIds {
    index: HashMap<Vec<u8>, u64>,
    bytes: Vec<Vec<u8>>,
}

fn long_ids() -> &'static Mutex<LongIds> {
    static LONG_IDS: OnceLock<Mutex<LongIds>> = OnceLock::new();
    LONG_IDS.get_or_init(|| Mutex::new(LongIds::default()))
}

impl Key {
    #[inline]
    pub fn from_bytes(b: &[u8]) -> Key {
        if b.len() <= PACKED_MAX {
            let mut value = 0u64;
            let mut packable = true;
            for &c in b {
                let code = CODE[c as usize];
                packable &= code != 0;
                value = (value << 6) | u64::from(code);
            }
            if packable {
                return Key(value << (6 * (PACKED_MAX - b.len())));
            }
        }
        Key::intern(b)
    }

    #[cold]
    fn intern(b: &[u8]) -> Key {
        let mut ids = long_ids().lock().unwrap();
        let next = ids.bytes.len() as u64;
        let index = *ids.index.entry(b.to_vec()).or_insert(next);
        if index == next {
            ids.bytes.push(b.to_vec());
        }
        Key(LONG | index)
    }

    pub fn to_vec(self) -> Vec<u8> {
        if self.0 & LONG == 0 {
            let mut out = Vec::with_capacity(PACKED_MAX);
            for i in (0..PACKED_MAX).rev() {
                let code = ((self.0 >> (6 * i)) & 63) as usize;
                if code == 0 {
                    break;
                }
                out.push(BYTE[code]);
            }
            out
        } else {
            let ids = long_ids().lock().unwrap();
            ids.bytes[(self.0 & !LONG) as usize].clone()
        }
    }

    pub fn cmp_bytes(self, other: Key) -> Ordering {
        if (self.0 | other.0) & LONG == 0 {
            self.0.cmp(&other.0)
        } else {
            self.to_vec().cmp(&other.to_vec())
        }
    }

    /// Spreads the key over `1 << bits` buckets.
    #[inline]
    pub fn bucket(self, bits: u32) -> usize {
        (self.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_orders_like_bytes() {
        let samples: Vec<&[u8]> = vec![
            b"", b"0", b"u1", b"U1", b" u1", b"u1 ", b"u10", b"u2", b"e3\r", b"c_12", b"C_12", b"z", b"zzzzzzzzzz",
            b"zzzzzzzzzzz", b"abcdefghij", b"abcdefghi", b"abcdefghijk", b"user_id_a\tuser_id_b", b"u\x00", b"p-1",
        ];
        for a in &samples {
            assert_eq!(Key::from_bytes(a).to_vec(), a.to_vec(), "{a:?}");
            for b in &samples {
                assert_eq!(Key::from_bytes(a).cmp_bytes(Key::from_bytes(b)), a.cmp(b), "{a:?} vs {b:?}");
                assert_eq!(Key::from_bytes(a) == Key::from_bytes(b), a == b, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn packed_order_matches_bytes_on_generator_ids() {
        let mut ids: Vec<Vec<u8>> = Vec::new();
        let mut x: u64 = 0x1234_5678;
        for _ in 0..2000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = 1 + (x >> 60) as usize % 10;
            let id: Vec<u8> = (0..len).map(|i| b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz"[((x >> (i * 6)) % 63) as usize]).collect();
            ids.push(id);
        }
        for a in &ids {
            for b in ids.iter().take(200) {
                assert_eq!(Key::from_bytes(a).0.cmp(&Key::from_bytes(b).0), a.cmp(b));
            }
        }
    }
}
