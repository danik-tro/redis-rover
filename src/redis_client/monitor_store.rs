//! What the MONITOR task collected: parsed lines, bounded by count and bytes.

use std::{collections::VecDeque, mem::size_of, time::Instant};

use super::bytes::text_from_bytes;

/// Arguments kept per line; the rest are only counted.
const MAX_ARGS: usize = 32;
/// Bytes kept per argument. A line can carry a value of hundreds of MiB.
const MAX_ARG_BYTES: usize = 1024;
/// Seconds kept by the rate ring.
pub const RATE_WINDOW: usize = 60;

/// One command the server reported, before it gets its sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    /// Server time, in microseconds since the Unix epoch.
    pub ts_micros: u64,
    pub db: i64,
    /// `ip:port`, `[::1]:port`, `unix:/path` or `lua`.
    pub client: String,
    pub cmd: String,
    /// The arguments after the command, each cut at [`MAX_ARG_BYTES`].
    pub args: Vec<String>,
    /// Arguments past [`MAX_ARGS`], not stored.
    pub more_args: usize,
    /// Some stored argument was cut.
    pub truncated: bool,
}

/// A stored line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLine {
    pub seq: u64,
    pub line: ParsedLine,
    /// What this line counts for against the byte limit.
    cost: usize,
}

/// Parse `1339518083.107412 [0 127.0.0.1:60866] "keys" "*"`. `None` when the
/// line has another shape.
#[must_use]
pub fn parse_line(raw: &str) -> Option<ParsedLine> {
    let (ts, rest) = raw.split_once(' ')?;
    let ts_micros = parse_ts(ts)?;
    let rest = rest.strip_prefix('[')?;
    let (db, rest) = rest.split_once(' ')?;
    let db = db.parse().ok()?;
    // An IPv6 client holds `]` itself, but never `] "`.
    let (client, rest) = rest.split_once("] \"")?;
    let mut args = Args {
        rest: rest.as_bytes(),
    };
    let (cmd, _) = args.next_arg()?;
    let cmd = text_from_bytes(&cmd);

    let mut kept = Vec::new();
    let (mut more_args, mut truncated) = (0, false);
    while args.open_next()? {
        let (arg, cut) = args.next_arg()?;
        if kept.len() < MAX_ARGS {
            truncated |= cut;
            kept.push(text_from_bytes(&arg));
        } else {
            more_args += 1;
        }
    }
    Some(ParsedLine {
        ts_micros,
        db,
        client: client.to_owned(),
        cmd,
        args: kept,
        more_args,
        truncated,
    })
}

/// `seconds.micros` as whole microseconds, without going through a float.
fn parse_ts(ts: &str) -> Option<u64> {
    let (secs, frac) = ts.split_once('.')?;
    if frac.is_empty() || frac.len() > 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = 10u64.pow(u32::try_from(6 - frac.len()).ok()?);
    let micros = frac.parse::<u64>().ok()?.checked_mul(scale)?;
    secs.parse::<u64>()
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(micros)
}

/// The quoted arguments of a line, the opening quote of the next one already
/// consumed.
struct Args<'a> {
    rest: &'a [u8],
}

impl Args<'_> {
    /// After an argument: `Some(true)` at ` "`, `Some(false)` at the end.
    fn open_next(&mut self) -> Option<bool> {
        match self.rest {
            [] => Some(false),
            [b' ', b'"', rest @ ..] => {
                self.rest = rest;
                Some(true)
            }
            _ => None,
        }
    }

    /// Decode up to the closing quote (`sdscatrepr` escapes). Returns the
    /// bytes, at most [`MAX_ARG_BYTES`], and whether it was cut.
    fn next_arg(&mut self) -> Option<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        let mut cut = false;
        let mut i = 0;
        loop {
            let byte = *self.rest.get(i)?;
            i += 1;
            let decoded = match byte {
                b'"' => break,
                b'\\' => {
                    let escape = *self.rest.get(i)?;
                    i += 1;
                    match escape {
                        b'\\' | b'"' => escape,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'a' => 0x07,
                        b'b' => 0x08,
                        b'x' => {
                            let high = hex(*self.rest.get(i)?)?;
                            let low = hex(*self.rest.get(i + 1)?)?;
                            i += 2;
                            high << 4 | low
                        }
                        _ => return None,
                    }
                }
                other => other,
            };
            if out.len() < MAX_ARG_BYTES {
                out.push(decoded);
            } else {
                cut = true;
            }
        }
        self.rest = &self.rest[i..];
        if cut {
            trim_cut_char(&mut out);
        }
        Some((out, cut))
    }
}

/// Drop a UTF-8 character the cut left incomplete at the end, so it doesn't
/// read as binary.
fn trim_cut_char(bytes: &mut Vec<u8>) {
    if let Err(err) = std::str::from_utf8(bytes) {
        if err.error_len().is_none() {
            bytes.truncate(err.valid_up_to());
        }
    }
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// Where the MONITOR connection stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MonitorStatus {
    #[default]
    Stopped,
    Connecting,
    Running,
    Reconnecting {
        attempt: u32,
        next_at: Instant,
    },
    /// The server refused MONITOR (an ACL, a managed service). Not retried.
    Failed(String),
}

/// Limits of the [`MonitorStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorLimits {
    pub lines: usize,
    pub bytes: usize,
}

/// The lines on screen. Written by the MONITOR task under a short lock, one
/// batch at a time; the UI redraws when [`MonitorStore::version`] moves.
#[derive(Debug)]
pub struct MonitorStore {
    items: VecDeque<MonitorLine>,
    bytes: usize,
    limits: MonitorLimits,
    next_seq: u64,
    status: MonitorStatus,
    version: u64,
    /// Lines the server sent, sampled out or not.
    seen: u64,
    /// `n` while one line in `n` is kept.
    sample_every: u32,
    /// Lines per second, indexed by Unix second % [`RATE_WINDOW`].
    buckets: [u64; RATE_WINDOW],
    /// Unix second of the newest bucket written.
    head: u64,
}

impl MonitorStore {
    #[must_use]
    pub fn new(limits: MonitorLimits) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            limits,
            next_seq: 1,
            status: MonitorStatus::Stopped,
            version: 0,
            seen: 0,
            sample_every: 1,
            buckets: [0; RATE_WINDOW],
            head: 0,
        }
    }

    /// Store a batch: `seen` lines arrived in second `sec`, `lines` are the
    /// ones kept while keeping one in `sample_every`.
    pub fn push_batch(&mut self, lines: Vec<ParsedLine>, seen: u64, sample_every: u32, sec: u64) {
        for line in lines {
            let cost = line_cost(&line);
            self.bytes += cost;
            self.items.push_back(MonitorLine {
                seq: self.next_seq,
                line,
                cost,
            });
            self.next_seq += 1;
        }
        while self.items.len() > self.limits.lines || self.bytes > self.limits.bytes {
            let Some(old) = self.items.pop_front() else {
                break;
            };
            self.bytes -= old.cost;
        }
        self.seen += seen;
        self.sample_every = sample_every.max(1);
        self.advance(sec);
        self.buckets[bucket(sec)] += seen;
        self.version += 1;
    }

    /// Zero the buckets of the seconds between the newest one and `sec`.
    fn advance(&mut self, sec: u64) {
        if sec <= self.head {
            return;
        }
        let gap = (sec - self.head).min(RATE_WINDOW as u64);
        for step in 1..=gap {
            self.buckets[bucket(self.head + step)] = 0;
        }
        self.head = sec;
    }

    /// Lines per second over the last [`RATE_WINDOW`] seconds up to `sec`,
    /// oldest first.
    #[must_use]
    pub fn rates(&self, sec: u64) -> Vec<u64> {
        let window = RATE_WINDOW as u64;
        (0..window)
            .rev()
            .map(|ago| {
                let at = sec.saturating_sub(ago);
                let held = at <= self.head && self.head - at < window;
                if held {
                    self.buckets[bucket(at)]
                } else {
                    0
                }
            })
            .collect()
    }

    /// Drop every line; sequence numbers keep counting.
    pub fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
        self.version += 1;
    }

    pub fn set_status(&mut self, status: MonitorStatus) {
        if status != self.status {
            self.status = status;
            self.version += 1;
        }
    }

    #[must_use]
    pub fn status(&self) -> &MonitorStatus {
        &self.status
    }

    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn seen(&self) -> u64 {
        self.seen
    }

    #[must_use]
    pub fn sample_every(&self) -> u32 {
        self.sample_every
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[must_use = "iterators do nothing unless used"]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &MonitorLine> {
        self.items.iter()
    }
}

#[allow(clippy::cast_possible_truncation)] // The remainder is below 60.
fn bucket(sec: u64) -> usize {
    (sec % RATE_WINDOW as u64) as usize
}

/// The heap a line holds, so the byte limit bounds real memory.
fn line_cost(line: &ParsedLine) -> usize {
    size_of::<MonitorLine>()
        + line.client.capacity()
        + line.cmd.capacity()
        + line.args.capacity() * size_of::<String>()
        + line.args.iter().map(String::capacity).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(lines: usize, bytes: usize) -> MonitorStore {
        MonitorStore::new(MonitorLimits { lines, bytes })
    }

    fn line(cmd: &str) -> ParsedLine {
        parse_line(&format!("1.000000 [0 lua] \"{cmd}\"")).unwrap()
    }

    #[test]
    fn parses_every_client_form() {
        for (raw, db, client) in [
            (
                r#"1339518083.107412 [0 127.0.0.1:60866] "keys" "*""#,
                0,
                "127.0.0.1:60866",
            ),
            (r#"1339518083.107412 [3 lua] "keys" "*""#, 3, "lua"),
            (
                r#"1339518083.107412 [15 unix:/tmp/redis.sock] "keys" "*""#,
                15,
                "unix:/tmp/redis.sock",
            ),
            (
                r#"1339518083.107412 [1 [::1]:6380] "keys" "*""#,
                1,
                "[::1]:6380",
            ),
        ] {
            let parsed = parse_line(raw).unwrap_or_else(|| panic!("{raw}"));
            assert_eq!(parsed.ts_micros, 1_339_518_083_107_412, "{raw}");
            assert_eq!(parsed.db, db, "{raw}");
            assert_eq!(parsed.client, client, "{raw}");
            assert_eq!(parsed.cmd, "keys", "{raw}");
            assert_eq!(parsed.args, ["*"], "{raw}");
        }
    }

    #[test]
    fn decodes_escapes_and_keeps_non_utf8_exact() {
        let parsed =
            parse_line(r#"1.5 [0 lua] "SET" "a\"b\\c\n\r\t\a\b" "\xff\x00" "caf\xc3\xa9""#)
                .unwrap();
        assert_eq!(parsed.ts_micros, 1_500_000);
        assert_eq!(parsed.args[0], "a\"b\\c\n\r\t\x07\x08");
        assert_eq!(parsed.args[1], text_from_bytes(&[0xff, 0x00]));
        assert_eq!(parsed.args[2], "café");
    }

    #[test]
    fn a_quote_or_bracket_inside_an_argument_is_data() {
        let parsed = parse_line(r#"1.0 [0 1.2.3.4:5] "SET" "x] \"y" "z""#).unwrap();
        assert_eq!(parsed.client, "1.2.3.4:5");
        assert_eq!(parsed.args, ["x] \"y", "z"]);
    }

    #[test]
    fn a_command_without_arguments() {
        let parsed = parse_line(r#"1.0 [0 lua] "PING""#).unwrap();
        assert_eq!(parsed.cmd, "PING");
        assert!(parsed.args.is_empty());
    }

    #[test]
    fn rejects_other_shapes() {
        for raw in [
            "",
            "OK",
            "x [0 lua] \"GET\"",
            "1.0 0 lua \"GET\"",
            "1.0 [x lua] \"GET\"",
            "1.0 [0 lua]",
            r#"1.0 [0 lua] "GET"#,
            r#"1.0 [0 lua] "GET" x"#,
            r#"1.0 [0 lua] "G\q""#,
            r#"1.0 [0 lua] "G\xZZ""#,
            r#"1.0 [0 lua] "G\x1"#,
            r#"1.1234567 [0 lua] "GET""#,
            r#"99999999999999999.0 [0 lua] "GET""#,
        ] {
            assert_eq!(parse_line(raw), None, "{raw}");
        }
    }

    #[test]
    fn caps_huge_arguments_and_argument_counts() {
        let big = "é".repeat(MAX_ARG_BYTES);
        let parsed = parse_line(&format!("1.0 [0 lua] \"SET\" \"{big}\"")).unwrap();
        assert!(parsed.truncated);
        assert!(parsed.args[0].len() <= MAX_ARG_BYTES);
        assert!(parsed.args[0].chars().all(|c| c == 'é'), "no broken char");

        let many = vec!["\"a\""; MAX_ARGS + 5].join(" ");
        let parsed = parse_line(&format!("1.0 [0 lua] \"DEL\" {many}")).unwrap();
        assert_eq!(parsed.args.len(), MAX_ARGS);
        assert_eq!(parsed.more_args, 5);
    }

    #[test]
    fn evicts_by_line_count_and_keeps_counting() {
        let mut s = store(2, usize::MAX);
        s.push_batch(vec![line("A"), line("B"), line("C")], 3, 1, 10);
        let cmds: Vec<_> = s.iter().map(|l| (l.seq, l.line.cmd.as_str())).collect();
        assert_eq!(cmds, [(2, "B"), (3, "C")]);
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.bytes(), 0);
        s.push_batch(vec![line("D")], 1, 1, 10);
        assert_eq!(s.iter().next().unwrap().seq, 4, "seq survives a clear");
    }

    #[test]
    fn evicts_by_bytes() {
        let one = line_cost(&line("A"));
        let mut s = store(100, one * 2);
        s.push_batch(vec![line("A"), line("B"), line("C")], 3, 1, 10);
        assert_eq!(s.len(), 2);
        assert!(s.bytes() <= one * 2);
    }

    #[test]
    fn rates_cover_gaps_and_expire() {
        let mut s = store(10, usize::MAX);
        s.push_batch(vec![], 5, 1, 1000);
        s.push_batch(vec![], 2, 1, 1000);
        s.push_batch(vec![], 3, 1, 1002);
        let rates = s.rates(1002);
        assert_eq!(rates.len(), RATE_WINDOW);
        assert_eq!(rates[RATE_WINDOW - 3..], [7, 0, 3]);
        assert_eq!(s.rates(1003)[RATE_WINDOW - 1], 0, "nothing this second");
        assert_eq!(s.rates(1002 + 60).iter().sum::<u64>(), 0, "all expired");
        // The same ring slot, a minute later, doesn't add to the old count.
        s.push_batch(vec![], 1, 1, 1060);
        assert_eq!(s.rates(1060)[RATE_WINDOW - 1], 1);
    }

    #[test]
    fn version_moves_on_every_change_a_frame_shows() {
        let mut s = store(10, usize::MAX);
        let v = s.version();
        s.set_status(MonitorStatus::Running);
        assert!(s.version() > v);
        let v = s.version();
        s.set_status(MonitorStatus::Running);
        assert_eq!(s.version(), v, "same status");
        s.push_batch(vec![line("A")], 1, 1, 1);
        assert!(s.version() > v);
        let v = s.version();
        s.clear();
        assert!(s.version() > v);
    }
}
