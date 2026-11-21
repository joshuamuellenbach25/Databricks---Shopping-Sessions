use hashbrown::HashMap;

use crate::ids::Key;

/// Linked IDs joined into persons. `name_of` gives the person's name (the
/// byte-wise smallest of its IDs); an ID in no link names itself.
pub struct People {
    names: HashMap<Key, Key>,
}

impl People {
    pub fn build(pairs: impl IntoIterator<Item = (Key, Key)>) -> People {
        let mut index: HashMap<Key, u32> = HashMap::new();
        let mut keys: Vec<Key> = Vec::new();
        let mut parent: Vec<u32> = Vec::new();
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
        let mut names = HashMap::with_capacity(keys.len());
        for i in 0..keys.len() as u32 {
            let root = find(&mut parent, i) as usize;
            let name = keys[best[root] as usize];
            if name != keys[i as usize] {
                names.insert(keys[i as usize], name);
            }
        }
        People { names }
    }

    pub fn name_of(&self, key: Key) -> Key {
        match self.names.get(&key) {
            Some(&name) => name,
            None => key,
        }
    }

    pub fn linked(&self) -> usize {
        self.names.len()
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
        let people = People::build(vec![(k("u3"), k("u9")), (k("u9"), k("u2")), (k("U7"), k("u9")), (k("u10"), k("u10"))]);
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
        let people = People::build(pairs);
        assert_eq!(people.name_of(k("c400")), k("c0"));
        assert_eq!(people.name_of(k("p999")), k("kiosk"));
        assert_eq!(people.name_of(k("c0")), k("c0"));
    }
}
