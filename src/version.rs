//! Version ordering of `sort -V` (GNU filevercmp, then byte order).

use std::cmp::Ordering;

/// Compares two version strings like `sort -V`: GNU filevercmp, with the
/// byte order breaking ties.
pub fn compare(a: &str, b: &str) -> Ordering {
    filevercmp(a.as_bytes(), b.as_bytes()).then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// The highest of some versions, as `sort -V | tail -n 1` would give.
pub fn newest<'a>(versions: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    versions.into_iter().max_by(|a, b| compare(a, b))
}

/// GNU filevercmp (gnulib).
pub fn filevercmp(a: &[u8], b: &[u8]) -> Ordering {
    if a.is_empty() || b.is_empty() {
        return (!a.is_empty()).cmp(&!b.is_empty());
    }
    // "." sorts first, then "..", then other names with a leading ".",
    // then other names
    if a[0] == b'.' {
        if b[0] != b'.' {
            return Ordering::Less;
        }
        let (adot, bdot) = (a.len() == 1, b.len() == 1);
        if adot || bdot {
            return (!adot).cmp(&!bdot);
        }
        let adotdot = a[1] == b'.' && a.len() == 2;
        let bdotdot = b[1] == b'.' && b.len() == 2;
        if adotdot || bdotdot {
            return (!adotdot).cmp(&!bdotdot);
        }
    } else if b[0] == b'.' {
        return Ordering::Greater;
    }

    let aprefix = file_prefixlen(a);
    let bprefix = file_prefixlen(b);
    let one_pass_only = aprefix == a.len() && bprefix == b.len();
    let result = verrevcmp(&a[..aprefix], &b[..bprefix]);
    if result != 0 || one_pass_only {
        return result.cmp(&0);
    }
    verrevcmp(a, b).cmp(&0)
}

/// Length of `s` without its file suffix: the longest match of
/// `(\.[A-Za-z~][A-Za-z0-9~]*)*$`.
fn file_prefixlen(s: &[u8]) -> usize {
    let n = s.len();
    let mut prefixlen = 0;
    let mut i = 0;
    loop {
        if i == n {
            return prefixlen;
        }
        i += 1;
        prefixlen = i;
        while i + 1 < n && s[i] == b'.' && (s[i + 1].is_ascii_alphabetic() || s[i + 1] == b'~') {
            i += 2;
            while i < n && (s[i].is_ascii_alphanumeric() || s[i] == b'~') {
                i += 1;
            }
        }
    }
}

fn order(s: &[u8], pos: usize) -> i32 {
    if pos >= s.len() {
        return 0;
    }
    let c = s[pos];
    if c.is_ascii_digit() {
        0
    } else if c.is_ascii_alphabetic() {
        c as i32
    } else if c == b'~' {
        -1
    } else {
        c as i32 + 256
    }
}

fn verrevcmp(s1: &[u8], s2: &[u8]) -> i32 {
    let (mut p1, mut p2) = (0, 0);
    let digit = |s: &[u8], p: usize| p < s.len() && s[p].is_ascii_digit();
    while p1 < s1.len() || p2 < s2.len() {
        let mut first_diff = 0;
        while (p1 < s1.len() && !digit(s1, p1)) || (p2 < s2.len() && !digit(s2, p2)) {
            let c1 = order(s1, p1);
            let c2 = order(s2, p2);
            if c1 != c2 {
                return c1 - c2;
            }
            p1 += 1;
            p2 += 1;
        }
        while p1 < s1.len() && s1[p1] == b'0' {
            p1 += 1;
        }
        while p2 < s2.len() && s2[p2] == b'0' {
            p2 += 1;
        }
        while digit(s1, p1) && digit(s2, p2) {
            if first_diff == 0 {
                first_diff = s1[p1] as i32 - s2[p2] as i32;
            }
            p1 += 1;
            p2 += 1;
        }
        if digit(s1, p1) {
            return 1;
        }
        if digit(s2, p2) {
            return -1;
        }
        if first_diff != 0 {
            return first_diff;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(list: &[&str]) -> Vec<String> {
        let mut v: Vec<&str> = list.to_vec();
        v.sort_by(|a, b| compare(a, b));
        v.into_iter().map(String::from).collect()
    }

    #[test]
    fn like_sort_v() {
        assert_eq!(compare("1.10-1", "1.9-1"), Ordering::Greater);
        assert_eq!(compare("1.0-2", "1.0-10"), Ordering::Less);
        assert_eq!(compare("2.0~rc1-1", "2.0-1"), Ordering::Less);
        assert_eq!(compare("1.0-1", "1.0-1"), Ordering::Equal);
        assert_eq!(compare("1.0a-1", "1.0-1"), Ordering::Less);
        assert_eq!(
            sorted(&["8.10-1", "8.2-1", "8.2-10", "8.2-2", "10.0-1"]),
            ["8.2-1", "8.2-2", "8.2-10", "8.10-1", "10.0-1"]
        );
        // Leading zeros: equal for filevercmp, ordered by bytes then
        assert_eq!(compare("1.01-1", "1.1-1"), Ordering::Less);
    }

    #[test]
    fn newest_version() {
        assert_eq!(newest(["1.2-1", "1.10-1", "1.9-3"]), Some("1.10-1"));
        assert_eq!(newest(Vec::<&str>::new()), None);
    }
}
