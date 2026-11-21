use memchr::memchr;

pub fn split<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let mut n = 0;
    let mut rest = line;
    loop {
        let (field, more) = match memchr(b',', rest) {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        if n == N {
            return false;
        }
        match unquote(field) {
            Some(value) => out[n] = value,
            None => return false,
        }
        n += 1;
        match more {
            Some(r) => rest = r,
            None => break,
        }
    }
    n == N
}

fn unquote(field: &[u8]) -> Option<&[u8]> {
    match field.first() {
        None => Some(field),
        Some(b'"') => {
            if field.len() < 2 || field[field.len() - 1] != b'"' {
                return None;
            }
            let inner = &field[1..field.len() - 1];
            if memchr(b'"', inner).is_some() {
                return None;
            }
            Some(inner)
        }
        Some(_) => {
            if memchr(b'"', field).is_some() {
                return None;
            }
            Some(field)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields2(line: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut out: [&[u8]; 2] = [&[]; 2];
        split(line, &mut out).then(|| out.iter().map(|f| f.to_vec()).collect())
    }

    #[test]
    fn follows_the_dialect() {
        assert_eq!(fields2(b"u1,u2"), Some(vec![b"u1".to_vec(), b"u2".to_vec()]));
        assert_eq!(fields2(b"\"u1\",u2"), Some(vec![b"u1".to_vec(), b"u2".to_vec()]));
        assert_eq!(fields2(b"\"\",u2"), Some(vec![b"".to_vec(), b"u2".to_vec()]));
        assert_eq!(fields2(b",u2"), Some(vec![b"".to_vec(), b"u2".to_vec()]));
        assert_eq!(fields2(b" u1,\" u1\""), Some(vec![b" u1".to_vec(), b" u1".to_vec()]));
        assert_eq!(fields2(b"u1,u2\r"), Some(vec![b"u1".to_vec(), b"u2\r".to_vec()]));
        assert_eq!(fields2(b"u1,"), Some(vec![b"u1".to_vec(), b"".to_vec()]));
        assert_eq!(fields2(b"u1,u2,u3"), None);
        assert_eq!(fields2(b"u1"), None);
        assert_eq!(fields2(b""), None);
        assert_eq!(fields2(b"u1,u5\""), None);
        assert_eq!(fields2(b"\"u1,u2"), None);
        assert_eq!(fields2(b"u\"1,u2"), None);
        assert_eq!(fields2(b"\",u2"), None);
        assert_eq!(fields2(b"\"u\"1\",u2"), None);
    }
}
