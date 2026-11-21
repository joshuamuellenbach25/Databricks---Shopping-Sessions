use std::cmp::Ordering;
use std::sync::{Mutex, OnceLock};

use hashbrown::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Key(pub u64);

pub const PACKED_MAX: usize = 10;
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

const FIELD_LOW: u64 = every_field(0x1F);
const FIELD_HIGH: u64 = every_field(0x20);

const fn every_field(bits: u64) -> u64 {
    let mut out = 0;
    let mut i = 0;
    while i < PACKED_MAX {
        out |= bits << (6 * i);
        i += 1;
    }
    out
}

#[inline(always)]
fn code(c: u8) -> u64 {
    u64::from(CODE[c as usize])
}

#[inline(always)]
fn short_word(b: &[u8]) -> u64 {
    let n = b.len();
    if n >= 4 {
        let first = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let last = u32::from_le_bytes([b[n - 4], b[n - 3], b[n - 2], b[n - 1]]);
        u64::from(first) | (u64::from(last) << (8 * (n - 4)))
    } else if n > 0 {
        u64::from(b[0]) | (u64::from(b[n / 2]) << (8 * (n / 2))) | (u64::from(b[n - 1]) << (8 * (n - 1)))
    } else {
        0
    }
}

#[inline(always)]
fn pack(b: &[u8]) -> Option<Key> {
    let n = b.len();
    if n > PACKED_MAX {
        return None;
    }
    let value = if n >= 8 {
        let mut value = 0;
        for (i, &c) in b[..8].iter().enumerate() {
            value |= code(c) << (6 * (PACKED_MAX - 1 - i));
        }
        let tail = u32::from(u16::from_le_bytes([b[n - 2], b[n - 1]])) >> (8 * (PACKED_MAX - n));
        value | (code(tail as u8) << 6) | code((tail >> 8) as u8)
    } else {
        let word = short_word(b);
        let mut value = 0;
        for i in 0..8 {
            value |= code((word >> (8 * i)) as u8) << (6 * (PACKED_MAX - 1 - i));
        }
        value
    };
    let nonzero = (((value & FIELD_LOW) + FIELD_LOW) | value) & FIELD_HIGH;
    let leading = FIELD_HIGH & !((1u64 << (6 * (PACKED_MAX - n))) - 1);
    (nonzero == leading).then_some(Key(value))
}

impl Key {
    #[inline]
    pub fn from_bytes(b: &[u8]) -> Key {
        match pack(b) {
            Some(key) => key,
            None => Key::intern(b),
        }
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
            self.append_to(&mut out);
            out
        } else {
            let ids = long_ids().lock().unwrap();
            ids.bytes[(self.0 & !LONG) as usize].clone()
        }
    }

    pub fn append_to(self, out: &mut Vec<u8>) {
        if self.0 & LONG == 0 {
            for i in (0..PACKED_MAX).rev() {
                let code = ((self.0 >> (6 * i)) & 63) as usize;
                if code == 0 {
                    break;
                }
                out.push(BYTE[code]);
            }
        } else {
            let ids = long_ids().lock().unwrap();
            out.extend_from_slice(&ids.bytes[(self.0 & !LONG) as usize]);
        }
    }

    pub fn cmp_bytes(self, other: Key) -> Ordering {
        if (self.0 | other.0) & LONG == 0 {
            self.0.cmp(&other.0)
        } else {
            self.to_vec().cmp(&other.to_vec())
        }
    }

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
    fn append_to_adds_the_bytes_of_packed_and_long_keys() {
        let samples: Vec<&[u8]> = vec![b"", b"0", b"u1", b"zzzzzzzzzz", b"zzzzzzzzzzz", b"abcdefghijk", b"user_id_a\tuser_id_b", b"u\x00", b"p-1"];
        let mut got = b"head".to_vec();
        let mut want = b"head".to_vec();
        for s in &samples {
            Key::from_bytes(s).append_to(&mut got);
            want.extend_from_slice(s);
            assert_eq!(got, want, "{s:?}");
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

    fn reference_pack(b: &[u8]) -> Option<u64> {
        if b.len() > PACKED_MAX {
            return None;
        }
        let mut value = 0u64;
        let mut packable = true;
        for &c in b {
            let code = CODE[c as usize];
            packable &= code != 0;
            value = (value << 6) | u64::from(code);
        }
        packable.then(|| value << (6 * (PACKED_MAX - b.len())))
    }

    fn agree(id: &[u8]) -> bool {
        let got = pack(id).map(|k| k.0);
        assert_eq!(got, reference_pack(id), "{id:?}");
        if let Some(value) = got {
            assert_eq!(Key::from_bytes(id), Key(value), "{id:?}");
        }
        got.is_some()
    }

    #[test]
    fn pack_agrees_with_the_reference_on_every_byte_at_every_position() {
        let base = b"azAZ09_m5Qx";
        let mut packed = 0;
        for len in 0..=base.len() {
            packed += usize::from(agree(&base[..len]));
            for at in 0..len {
                for c in 0..=255u8 {
                    let mut id = base[..len].to_vec();
                    id[at] = c;
                    packed += usize::from(agree(&id));
                }
            }
        }
        assert_eq!(packed, 1 + PACKED_MAX + 63 * 55);
    }

    #[test]
    fn pack_agrees_with_the_reference_on_random_ids() {
        let alphabet = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";
        let edges = [0x00, 0x09, 0x0D, b' ', b'"', b'-', b'/', b':', b'@', b'[', b'^', b'`', b'{', 0x7F, 0x80, 0xFF];
        let mut x: u64 = 99;
        let mut packed = 0;
        for _ in 0..1_000_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let len = (x >> 60) as usize % (PACKED_MAX + 3);
            let mut y = x;
            let id: Vec<u8> = (0..len)
                .map(|_| {
                    y = y.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    if (y >> 59) % 10 == 0 {
                        edges[(y >> 40) as usize % edges.len()]
                    } else {
                        alphabet[(y >> 40) as usize % alphabet.len()]
                    }
                })
                .collect();
            packed += usize::from(agree(&id));
        }
        assert!(packed > 200_000, "{packed}");
    }
}
