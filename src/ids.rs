use std::cmp::Ordering;
use std::sync::{OnceLock, RwLock, RwLockReadGuard};

use hashbrown::HashMap;

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

fn long_ids() -> &'static RwLock<LongIds> {
    static LONG_IDS: OnceLock<RwLock<LongIds>> = OnceLock::new();
    LONG_IDS.get_or_init(|| RwLock::new(LongIds::default()))
}

pub struct Spelling(RwLockReadGuard<'static, LongIds>);

pub fn spelling() -> Spelling {
    Spelling(long_ids().read().unwrap())
}

impl Spelling {
    #[inline]
    pub fn append(&self, key: Key, out: &mut Vec<u8>) {
        if key.0 & LONG == 0 {
            append_packed(key.0, out);
        } else {
            out.extend_from_slice(&self.0.bytes[(key.0 & !LONG) as usize]);
        }
    }
}

#[inline]
fn append_packed(value: u64, out: &mut Vec<u8>) {
    for i in (0..PACKED_MAX).rev() {
        let code = ((value >> (6 * i)) & 63) as usize;
        if code == 0 {
            break;
        }
        out.push(BYTE[code]);
    }
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

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
const fn nibble_tables() -> [[u8; 16]; 3] {
    let mut tables = [[0u8; 16]; 3];
    let mut n = 0;
    while n < 16 {
        let mut low = 0x10;
        if n >= 10 {
            low |= 0x01;
        }
        if n == 0 {
            low |= 0x02;
        }
        if n >= 11 && n <= 14 {
            low |= 0x04;
        }
        if n >= 11 {
            low |= 0x08;
        }
        tables[0][n] = low;
        let (high, shift): (u8, i32) = match n {
            3 => (0x01, -47),
            4 => (0x02, -54),
            5 => (0x04, -54),
            6 => (0x02, -59),
            7 => (0x08, -59),
            _ => (0x10, 0),
        };
        tables[1][n] = high;
        tables[2][n] = shift as u8;
        n += 1;
    }
    tables
}

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
static NIBBLES: [[u8; 16]; 3] = nibble_tables();
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
static PREFIX: [u8; 32] = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
pub unsafe fn pack_two(a: &[u8], b: &[u8]) -> (Option<Key>, Option<Key>) {
    use std::arch::x86_64::*;
    let pair = |x: *const u8, y: *const u8| _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_loadu_si128(x.cast())), _mm_loadu_si128(y.cast()));
    let table = |t: &[u8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(t.as_ptr().cast()));
    let bytes = pair(a.as_ptr(), b.as_ptr());
    let inside = pair(PREFIX.as_ptr().add(16 - a.len().min(16)), PREFIX.as_ptr().add(16 - b.len().min(16)));
    let nibble = _mm256_set1_epi8(0x0F);
    let low = _mm256_and_si256(bytes, nibble);
    let high = _mm256_and_si256(_mm256_srli_epi16::<4>(bytes), nibble);
    let bad = _mm256_and_si256(_mm256_and_si256(_mm256_shuffle_epi8(table(&NIBBLES[0]), low), _mm256_shuffle_epi8(table(&NIBBLES[1]), high)), inside);
    let underscore = _mm256_and_si256(_mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(b'_' as i8)), _mm256_set1_epi8(4));
    let codes = _mm256_and_si256(_mm256_sub_epi8(_mm256_add_epi8(bytes, _mm256_shuffle_epi8(table(&NIBBLES[2]), high)), underscore), inside);
    let pairs = _mm256_maddubs_epi16(codes, _mm256_set1_epi16(0x0140));
    let quads = _mm256_madd_epi16(pairs, _mm256_set1_epi32(0x0001_1000));
    let top = _mm256_slli_epi64::<36>(quads);
    let middle = _mm256_andnot_si256(_mm256_set1_epi64x(0xFFF), _mm256_srli_epi64::<20>(quads));
    let tail = _mm256_srli_epi64::<12>(_mm256_srli_si256::<8>(quads));
    let values = _mm256_or_si256(_mm256_or_si256(top, middle), tail);
    let clean = _mm256_movemask_epi8(_mm256_cmpeq_epi8(bad, _mm256_setzero_si256())) as u32;
    (
        (clean & 0xFFFF == 0xFFFF && a.len() <= PACKED_MAX).then_some(Key(_mm256_extract_epi64::<0>(values) as u64)),
        (clean >> 16 == 0xFFFF && b.len() <= PACKED_MAX).then_some(Key(_mm256_extract_epi64::<2>(values) as u64)),
    )
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
pub unsafe fn pack_two(a: &[u8], b: &[u8]) -> (Option<Key>, Option<Key>) {
    (pack(a), pack(b))
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
        let mut ids = long_ids().write().unwrap();
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
            let ids = long_ids().read().unwrap();
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
            let key = Key::from_bytes(a);
            let mut spelled = b"x".to_vec();
            spelling().append(key, &mut spelled);
            assert_eq!(spelled[1..], a[..], "{a:?}");
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

    fn agree_two(a: &[u8], b: &[u8], fill: u8) {
        let padded = |id: &[u8]| [id, &[fill; 32]].concat();
        let (x, y) = (padded(a), padded(b));
        let got = unsafe { pack_two(&x[..a.len()], &y[..b.len()]) };
        assert_eq!(got, (pack(a), pack(b)), "{a:?} {b:?} fill {fill}");
    }

    #[test]
    fn pack_two_agrees_with_pack_on_every_byte_at_every_position() {
        let base = b"azAZ09_m5QxB7";
        let fills = [b'a', b'0', b'_', b'"', b',', 0x00, 0xFF, b'Z', b'z', b'9'];
        let mut checked = 0;
        for len in 0..=base.len() {
            let other = &base[base.len() - len..];
            agree_two(&base[..len], other, fills[len % fills.len()]);
            for at in 0..len {
                for c in 0..=255u8 {
                    let mut id = base[..len].to_vec();
                    id[at] = c;
                    agree_two(&id, other, fills[checked % fills.len()]);
                    agree_two(other, &id, fills[(checked + 3) % fills.len()]);
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 256 * (1..=base.len()).sum::<usize>());
    }

    #[test]
    fn pack_two_agrees_with_pack_on_random_ids() {
        let alphabet = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ_abcdefghijklmnopqrstuvwxyz";
        let edges = [0x00, 0x09, 0x0D, b' ', b'"', b',', b'-', b'/', b':', b'@', b'[', b'^', b'`', b'{', 0x7F, 0x80, 0xFF];
        let mut x: u64 = 7;
        let mut next = move |below: usize| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((x >> 33) % below as u64) as usize
        };
        let mut packed = 0;
        for _ in 0..300_000 {
            let mut ids: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
            for id in ids.iter_mut() {
                let len = next(PACKED_MAX + 4);
                for _ in 0..len {
                    let c = if next(12) == 0 { edges[next(edges.len())] } else { alphabet[next(alphabet.len())] };
                    id.push(c);
                }
            }
            let fill = if next(2) == 0 { alphabet[next(alphabet.len())] } else { edges[next(edges.len())] };
            agree_two(&ids[0], &ids[1], fill);
            packed += usize::from(pack(&ids[0]).is_some());
        }
        assert!(packed > 50_000, "{packed}");
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
