//! Redis glob patterns (`SCAN … MATCH`, `KEYS`) matched on the client, the
//! way the server's `stringmatchlen` does: `*`, `?`, `[abc]`, `[^a-z]` and
//! `\` escapes, byte by byte.

/// `name` matches the glob `pattern`.
#[must_use]
pub fn glob_match(pattern: &str, name: &str) -> bool {
    matches(pattern.as_bytes(), name.as_bytes())
}

fn matches(mut pattern: &[u8], mut name: &[u8]) -> bool {
    while let Some(&p) = pattern.first() {
        match p {
            b'*' => {
                while pattern.get(1) == Some(&b'*') {
                    pattern = &pattern[1..];
                }
                if pattern.len() == 1 {
                    return true;
                }
                return (0..=name.len()).any(|skip| matches(&pattern[1..], &name[skip..]));
            }
            b'?' => {
                if name.is_empty() {
                    return false;
                }
                name = &name[1..];
            }
            b'[' => {
                let Some((&c, rest)) = name.split_first() else {
                    return false;
                };
                let (hit, after) = class(&pattern[1..], c);
                if !hit {
                    return false;
                }
                name = rest;
                // `after` starts at the closing `]` (or is empty).
                pattern = after;
                if pattern.is_empty() {
                    break;
                }
            }
            _ => {
                let literal = if p == b'\\' && pattern.len() >= 2 {
                    pattern = &pattern[1..];
                    pattern[0]
                } else {
                    p
                };
                if name.first() != Some(&literal) {
                    return false;
                }
                name = &name[1..];
            }
        }
        pattern = &pattern[1..];
    }
    name.is_empty()
}

/// Whether `c` is in the class that starts at `pattern` (just past `[`).
/// Also returns the pattern from the closing `]` on; an unclosed class runs
/// to the end of the pattern, as in Redis.
fn class(mut pattern: &[u8], c: u8) -> (bool, &[u8]) {
    let negate = pattern.first() == Some(&b'^');
    if negate {
        pattern = &pattern[1..];
    }
    let mut hit = false;
    loop {
        match pattern {
            [] | [b']', ..] => break,
            [b'\\', escaped, rest @ ..] => {
                hit |= *escaped == c;
                pattern = rest;
            }
            [low, b'-', high, rest @ ..] if *high != b']' => {
                let (low, high) = if low <= high {
                    (*low, *high)
                } else {
                    (*high, *low)
                };
                hit |= (low..=high).contains(&c);
                pattern = rest;
            }
            [one, rest @ ..] => {
                hit |= *one == c;
                pattern = rest;
            }
        }
    }
    (hit != negate, pattern)
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn matches_like_redis() {
        let cases = [
            ("*", "anything", true),
            ("*", "", true),
            ("user:*", "user:1", true),
            ("user:*", "order:1", false),
            ("user:*:name", "user:1:name", true),
            ("user:*:name", "user:1:email", false),
            ("**a", "ba", true),
            ("h?llo", "hello", true),
            ("h?llo", "hllo", false),
            ("h[ae]llo", "hallo", true),
            ("h[ae]llo", "hillo", false),
            ("h[^e]llo", "hallo", true),
            ("h[^e]llo", "hello", false),
            ("h[a-b]llo", "hbllo", true),
            ("h[b-a]llo", "hallo", true),
            ("h[a-b]llo", "hcllo", false),
            (r"a\*b", "a*b", true),
            (r"a\*b", "axb", false),
            (r"[\]]", "]", true),
            ("abc", "abcd", false),
            ("abc", "ab", false),
            ("[abc", "b", true),
        ];
        for (pattern, name, expected) in cases {
            assert_eq!(glob_match(pattern, name), expected, "{pattern} ~ {name}");
        }
    }
}
