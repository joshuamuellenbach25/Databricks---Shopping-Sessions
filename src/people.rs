use hashbrown::HashMap;

use crate::ids::Key;

pub struct People {
    slots: Vec<[u64; 2]>,
    mask: usize,
    shift: u32,
    linked: usize,
    filter: Vec<u64>,
    filter_mask: usize,
}

const SLOT_MIX: u64 = 0xD6E8_FEB8_6659_FD93;
const FILTER_MIX: u64 = 0x9E6C_63D0_676A_9A99;
const FILTER_BITS_PER_ID: usize = 8;

impl People {
    pub fn build(pairs: impl IntoIterator<Item = (Key, Key)>, expected: usize) -> People {
        let mut index: HashMap<Key, u32> = HashMap::with_capacity(expected * 2);
        let mut keys: Vec<Key> = Vec::with_capacity(expected * 2);
        let mut parent: Vec<u32> = Vec::with_capacity(expected * 2);
        let mut intern = |key: Key, keys: &mut Vec<Key>, parent: &mut Vec<u32>| -> u32 {
            *index.entry(key).or_insert_with(|| {
                keys.push(key);
                parent.push(keys.len() as u32 - 1);
                keys.len() as u32 - 1
            })
        };
        for (a, b) in pairs {
            let a = intern(a, &mut keys, &mut parent);
            let b = intern(b, &mut keys, &mut parent);
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra != rb {
                parent[ra as usize] = rb;
            }
        }
        let mut best: Vec<u32> = (0..keys.len() as u32).collect();
        for i in 0..keys.len() as u32 {
            let root = find(&mut parent, i) as usize;
            if keys[i as usize].cmp_bytes(keys[best[root] as usize]).is_lt() {
                best[root] = i;
            }
        }
        let mut names: Vec<(Key, Key)> = Vec::with_capacity(keys.len());
        for i in 0..keys.len() as u32 {
            let root = find(&mut parent, i) as usize;
            let name = keys[best[root] as usize];
            if name != keys[i as usize] {
                names.push((keys[i as usize], name));
            }
        }
        let capacity = (names.len() * 2).next_power_of_two().max(16);
        let words = (names.len() * FILTER_BITS_PER_ID / 64).next_power_of_two();
        let mut people = People {
            slots: vec![[0, 0]; capacity],
            mask: capacity - 1,
            shift: 64 - capacity.trailing_zeros(),
            linked: names.len(),
            filter: vec![0; words],
            filter_mask: words - 1,
        };
        for (id, name) in names {
            let (word, bits) = people.filter_bits(id);
            people.filter[word] |= bits;
            let mut i = people.slot(id);
            while people.slots[i][0] != 0 {
                i = (i + 1) & people.mask;
            }
            people.slots[i] = [id.0, name.0];
        }
        people
    }

    #[inline(always)]
    fn slot(&self, key: Key) -> usize {
        (key.0.wrapping_mul(SLOT_MIX) >> self.shift) as usize
    }

    #[inline(always)]
    fn filter_bits(&self, key: Key) -> (usize, u64) {
        let product = u128::from(key.0) * u128::from(FILTER_MIX);
        let hash = (product >> 64) as u64 ^ product as u64;
        (hash as usize & self.filter_mask, (1 << (hash >> 58)) | (1 << ((hash >> 52) & 63)))
    }

    #[inline(always)]
    pub fn may_be_linked(&self, key: Key) -> bool {
        let (word, bits) = self.filter_bits(key);
        self.filter[word] & bits == bits
    }

    #[inline(always)]
    pub fn prefetch_slot(&self, key: Key) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            _mm_prefetch::<_MM_HINT_T0>(self.slots.as_ptr().add(self.slot(key)) as *const i8);
        }
        #[cfg(not(target_arch = "x86_64"))]
        let _ = key;
    }

    #[cfg(test)]
    pub fn name_of(&self, key: Key) -> Key {
        if self.may_be_linked(key) {
            self.linked_name(key)
        } else {
            key
        }
    }

    #[inline(always)]
    pub fn linked_name(&self, key: Key) -> Key {
        let mut i = self.slot(key);
        loop {
            let [id, name] = self.slots[i];
            if id == key.0 {
                return if id == 0 { key } else { Key(name) };
            }
            if id == 0 {
                return key;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn linked(&self) -> usize {
        self.linked
    }
}

fn find(parent: &mut [u32], mut i: u32) -> u32 {
    while parent[i as usize] != i {
        let grand = parent[parent[i as usize] as usize];
        parent[i as usize] = grand;
        i = grand;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        Key::from_bytes(s.as_bytes())
    }

    #[test]
    fn chains_across_order_and_direction() {
        let people = People::build(vec![(k("u3"), k("u9")), (k("u9"), k("u2")), (k("U7"), k("u9")), (k("u10"), k("u10"))], 4);
        for id in ["u3", "u9", "u2", "U7"] {
            assert_eq!(people.name_of(k(id)), k("U7"), "{id}");
        }
        assert_eq!(people.name_of(k("u10")), k("u10"));
        assert_eq!(people.name_of(k("u1")), k("u1"));
    }

    #[test]
    fn long_chain_and_star() {
        let mut pairs = Vec::new();
        for i in 0..400 {
            pairs.push((k(&format!("c{}", i + 1)), k(&format!("c{i}"))));
        }
        for i in 0..1000 {
            pairs.push((k(&format!("p{i}")), k("kiosk")));
        }
        let people = People::build(pairs, 1400);
        assert_eq!(people.name_of(k("c400")), k("c0"));
        assert_eq!(people.name_of(k("p999")), k("kiosk"));
        assert_eq!(people.name_of(k("c0")), k("c0"));
    }

    #[test]
    fn table_matches_a_plain_map_on_many_ids() {
        let mut pairs = Vec::new();
        let mut x: u64 = 7;
        for _ in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = format!("u{}", (x >> 40) % 30_000);
            let b = format!("u{}", (x >> 10) % 30_000);
            pairs.push((k(&a), k(&b)));
        }
        let people = People::build(pairs.clone(), pairs.len());
        let mut parent: HashMap<String, String> = HashMap::new();
        fn root(parent: &mut HashMap<String, String>, x: &str) -> String {
            let mut cur = x.to_string();
            while let Some(next) = parent.get(&cur).cloned() {
                if next == cur {
                    break;
                }
                cur = next;
            }
            cur
        }
        let mut x: u64 = 7;
        for _ in 0..20_000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = format!("u{}", (x >> 40) % 30_000);
            let b = format!("u{}", (x >> 10) % 30_000);
            parent.entry(a.clone()).or_insert_with(|| a.clone());
            parent.entry(b.clone()).or_insert_with(|| b.clone());
            let (ra, rb) = (root(&mut parent, &a), root(&mut parent, &b));
            if ra != rb {
                parent.insert(ra, rb);
            }
        }
        let ids: Vec<String> = parent.keys().cloned().collect();
        let mut smallest: HashMap<String, String> = HashMap::new();
        for id in &ids {
            let r = root(&mut parent, id);
            let entry = smallest.entry(r).or_insert_with(|| id.clone());
            if id.as_bytes() < entry.as_bytes() {
                *entry = id.clone();
            }
        }
        for id in &ids {
            let r = root(&mut parent, id);
            assert_eq!(people.name_of(k(id)), k(&smallest[&r]), "{id}");
        }
        assert_eq!(people.name_of(k("u99999")), k("u99999"));
        assert_eq!(people.name_of(k("")), k(""));
    }
}
