//! The globs policy rules and audit filters are written in: `*` stands for any run of characters,
//! dots included, and `?` for exactly one.

/// Whether `name` matches `pattern`.
pub fn matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    // Where the last `*` was, and how much of the name it has taken so far, to come back to when
    // what followed it stops matching.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    p = star_p + 1;
                    n = star_n + 1;
                    star = Some((star_p, star_n + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn stars_and_question_marks() {
        assert!(matches("fs.read", "fs.read"));
        assert!(!matches("fs.read", "fs.readx"));
        assert!(matches("fs.*", "fs.read"));
        assert!(!matches("fs.*", "sys.info"));
        assert!(matches("*", "anything.at.all"));
        assert!(matches("*.list", "job.list"));
        assert!(matches("p*r.*", "power.sleep"));
        assert!(matches("job.?ait", "job.wait"));
        assert!(!matches("job.?ait", "job.ait"));
        assert!(matches("a*b*c", "aXbYbZc"));
        assert!(!matches("a*b*c", "aXbYbZ"));
        assert!(matches("**", ""));
        assert!(!matches("?", ""));
    }
}
