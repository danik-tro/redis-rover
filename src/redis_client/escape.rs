//! Commands as `redis-cli` would show them, for the write preview.

use std::{borrow::Cow, fmt::Write as _};

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

/// [`format_cmd`] showing at most `max_args` arguments after the command
/// name, then `…(+N more)`: a bulk write's line stays readable.
#[must_use]
pub fn format_cmd_brief(cmd: &Cmd, max_args: usize) -> String {
    let total = cmd.args_iter().count().saturating_sub(1);
    if total <= max_args {
        return format_cmd(cmd);
    }
    let mut out = String::new();
    for (i, arg) in cmd.args_iter().take(max_args + 1).enumerate() {
        let bytes = match arg {
            Arg::Simple(bytes) => bytes,
            _ => b"?".as_slice(),
        };
        if i == 0 {
            out.push_str(&String::from_utf8_lossy(bytes));
        } else {
            out.push(' ');
            out.push_str(&quote_arg(bytes));
        }
    }
    let _ = write!(out, " …(+{} more)", total - max_args);
    out
}

/// A character that changes how the text around it is drawn without being
/// visible itself: a control character, a Unicode format character (`Cf`:
/// bidi overrides and isolates, zero-width space / joiner, soft hyphen,
/// BOM, tag characters) or a line / paragraph separator. A terminal acts on
/// the first kind (escape sequences); the others let two different names
/// look the same, or one name read as another.
#[must_use]
pub fn is_hidden_char(c: char) -> bool {
    c.is_control()
        || super::bytes::byte_of(c).is_some()
        || matches!(
            c,
            '\u{ad}'
                | '\u{600}'..='\u{605}'
                | '\u{61c}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{890}'..='\u{891}'
                | '\u{8e2}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0001}'
                | '\u{e0020}'..='\u{e007f}'
        )
}

/// `c` as an escape: `\n`, `\r`, `\t`, `\x1b` for a control character
/// below U+0080, `\u{202e}` for the rest.
fn push_hidden(out: &mut String, c: char) {
    // A byte that isn't UTF-8 (`bytes.rs`), as `redis-cli` shows it.
    if let Some(byte) = super::bytes::byte_of(c) {
        let _ = write!(out, "\\x{byte:02x}");
        return;
    }
    match c {
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        c if c.is_ascii() => {
            let _ = write!(out, "\\x{:02x}", u32::from(c));
        }
        c => {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        }
    }
}

/// Text from the server (a key name, a version string, an error) made safe
/// to show: every [`is_hidden_char`] is written as an escape, so it can't
/// move the cursor, write the clipboard (OSC 52) or hide part of a name.
/// Everything else, a backslash included, is left as it is.
#[must_use]
pub fn printable(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_hidden_char) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if is_hidden_char(c) {
            push_hidden(&mut out, c);
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// [`printable`] for text that may span lines (an error with the stderr
/// of `password_command`): line breaks stay, everything else is escaped.
#[must_use]
pub fn printable_lines(text: &str) -> String {
    text.split('\n')
        .map(printable)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One argument in double quotes, escaped like `redis-cli` does: `\\`,
/// `\"`, `\n`, `\r`, `\t`, `\a`, `\b`; other control characters and bytes
/// that aren't UTF-8 as `\xHH`, invisible format characters (bidi overrides,
/// zero-width space) as `\u{202e}`. Printable Unicode stays readable. Past
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
                c if is_hidden_char(c) => push_hidden(&mut out, c),
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

    use super::{format_cmd, human_bytes, printable, quote_arg};

    /// #71 / #72: nothing the terminal acts on, and nothing invisible,
    /// survives.
    #[test]
    fn printable_escapes_hidden_characters() {
        let cases = [
            ("user:1", "user:1"),
            ("café ☕ キー", "café ☕ キー"),
            (r"C:\tmp", r"C:\tmp"),
            ("a\nb\r\tc", r"a\nb\r\tc"),
            ("\u{1b}]52;c;aGk=\u{7}", r"\x1b]52;c;aGk=\x07"),
            ("\u{9b}31m", r"\u{9b}31m"),
            ("pay\u{202e}txt.exe", r"pay\u{202e}txt.exe"),
            ("ad\u{200b}min", r"ad\u{200b}min"),
            ("\u{feff}x\u{7f}", r"\u{feff}x\x7f"),
        ];
        for (raw, shown) in cases {
            assert_eq!(printable(raw), shown, "{raw:?}");
            assert!(
                !printable(raw).chars().any(super::is_hidden_char),
                "{raw:?}"
            );
        }
        assert!(matches!(printable("plain"), std::borrow::Cow::Borrowed(_)));
        assert_eq!(
            super::printable_lines("one\ntwo\u{1b}[2J\r\n"),
            "one\ntwo\\x1b[2J\\r\n"
        );
        // The write preview shows them too.
        assert_eq!(
            quote_arg("ad\u{200b}min\u{202e}".as_bytes()),
            r#""ad\u{200b}min\u{202e}""#
        );
    }

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
