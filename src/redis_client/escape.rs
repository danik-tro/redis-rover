//! Commands as `redis-cli` would show them, for the write preview.

use std::fmt::Write as _;

use redis::{Arg, Cmd};

/// Bytes of an argument shown before it is cut.
pub const PREVIEW_ARG_BYTES: usize = 64;

/// `HSET "user:1" "name" "Ada"`: the command name bare, every argument
/// quoted by [`quote_arg`].
#[must_use]
pub fn format_cmd(cmd: &Cmd) -> String {
    let mut out = String::new();
    for (i, arg) in cmd.args_iter().enumerate() {
        let bytes = match arg {
            Arg::Simple(bytes) => bytes,
            // Only SCAN-style commands have a cursor; no write does.
            _ => b"?".as_slice(),
        };
        if i == 0 {
            out.push_str(&String::from_utf8_lossy(bytes));
        } else {
            out.push(' ');
            out.push_str(&quote_arg(bytes));
        }
    }
    out
}

/// One argument in double quotes, escaped like `redis-cli` does: `\\`,
/// `\"`, `\n`, `\r`, `\t`, `\a`, `\b`; other control characters and bytes
/// that aren't UTF-8 as `\xHH`. Printable Unicode stays readable. Past
/// [`PREVIEW_ARG_BYTES`] it is cut (on a character boundary) and ends in
/// `…(2.1 KiB more)`.
#[must_use]
pub fn quote_arg(bytes: &[u8]) -> String {
    let (shown, rest) = cut(bytes, PREVIEW_ARG_BYTES);
    let mut out = String::with_capacity(shown.len() + 2);
    out.push('"');
    for chunk in shown.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\u{7}' => out.push_str("\\a"),
                '\u{8}' => out.push_str("\\b"),
                c if c.is_control() => {
                    let mut buf = [0; 4];
                    for b in c.encode_utf8(&mut buf).bytes() {
                        let _ = write!(out, "\\x{b:02x}");
                    }
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    out.push('"');
    if rest > 0 {
        let _ = write!(out, "…({} more)", human_bytes(rest));
    }
    out
}

/// The first `max` bytes (backed off to a character boundary when the
/// bytes are UTF-8 there) and how many bytes were left out.
fn cut(bytes: &[u8], max: usize) -> (&[u8], usize) {
    if bytes.len() <= max {
        return (bytes, 0);
    }
    let mut end = max;
    // A UTF-8 continuation byte (10xx_xxxx) is never where a char starts.
    while end > 0 && end > max.saturating_sub(3) && bytes[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    if end == 0 || bytes[end] & 0xC0 == 0x80 {
        end = max;
    }
    (&bytes[..end], bytes.len() - end)
}

/// `512 B`, `2.1 KiB`, `3.0 MiB`.
#[allow(clippy::cast_precision_loss)] // one decimal shown
fn human_bytes(n: usize) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::{format_cmd, human_bytes, quote_arg};

    #[test]
    fn quotes_and_escapes_like_redis_cli() {
        let cases: &[(&[u8], &str)] = &[
            (b"Ada", r#""Ada""#),
            (b"", r#""""#),
            (b"two words", r#""two words""#),
            (br#"say "hi""#, r#""say \"hi\"""#),
            (br"C:\tmp", r#""C:\\tmp""#),
            (b"a\nb\r\tc", r#""a\nb\r\tc""#),
            (b"\x07\x08", r#""\a\b""#),
            (b"\x00\x1b[0m", r#""\x00\x1b[0m""#),
            (b"\xff\xfe ok", r#""\xff\xfe ok""#),
            ("café ☕".as_bytes(), "\"café ☕\""),
            ("\u{85}".as_bytes(), r#""\xc2\x85""#),
        ];
        for (raw, shown) in cases {
            assert_eq!(quote_arg(raw), *shown, "{raw:?}");
        }
    }

    #[test]
    fn long_values_are_cut_with_the_rest_counted() {
        let value = "x".repeat(64 + 2150);
        assert_eq!(
            quote_arg(value.as_bytes()),
            format!("\"{}\"…(2.1 KiB more)", "x".repeat(64))
        );
        let exactly = "y".repeat(64);
        assert_eq!(quote_arg(exactly.as_bytes()), format!("\"{exactly}\""));
        assert_eq!(
            quote_arg("z".repeat(65).as_bytes()),
            format!("\"{}\"…(1 B more)", "z".repeat(64))
        );
    }

    #[test]
    fn cut_never_splits_a_character() {
        // 63 ASCII bytes, then a 3-byte '€' across the 64-byte limit.
        let value = format!("{}€€", "a".repeat(63));
        assert_eq!(
            quote_arg(value.as_bytes()),
            format!("\"{}\"…(6 B more)", "a".repeat(63))
        );
        // Binary data is cut at the byte limit.
        let binary = [0xffu8; 70];
        assert_eq!(
            quote_arg(&binary),
            format!("\"{}\"…(6 B more)", "\\xff".repeat(64))
        );
    }

    #[test]
    fn sizes_read_like_humans() {
        assert_eq!(human_bytes(1), "1 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn commands_show_the_name_bare() {
        assert_eq!(
            format_cmd(&redis::Cmd::hset("user:1", "name", "Ada")),
            r#"HSET "user:1" "name" "Ada""#
        );
    }
}
