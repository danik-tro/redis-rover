//! In-memory state of the Pub/Sub tab: subscriptions, their bounded message
//! buffers and stats. Written by the Pub/Sub task, read by the UI.

use std::{
    collections::{HashMap, VecDeque},
    fmt::{self, Write as _},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::pubsub_notify::NotifyState;

/// Seconds kept by the per-second rate ring.
const RATE_WINDOW_SECS: u32 = 60;
const RATE_WINDOW: usize = RATE_WINDOW_SECS as usize;
/// Bytes shown in a binary payload's hex preview.
const HEX_PREVIEW: usize = 16;
/// Bytes inspected by the binary-payload check.
const BINARY_SNIFF: usize = 256;
/// Bytes per row of a hex dump.
const HEX_ROW: usize = 16;
/// Concrete channels counted by name per pattern subscription; the rest go
/// into [`SubStats::other`].
pub const PER_CHANNEL_CAP: usize = 1_000;

/// What a subscription listens to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    /// `SUBSCRIBE channel`
    Channel(String),
    /// `PSUBSCRIBE pattern`
    Pattern(String),
}

impl Target {
    /// Build a target of `kind` named `name`.
    #[must_use]
    pub fn new(kind: TargetKind, name: String) -> Self {
        match kind {
            TargetKind::Channel => Self::Channel(name),
            TargetKind::Pattern => Self::Pattern(name),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Channel(name) | Self::Pattern(name) => name,
        }
    }

    #[must_use]
    pub fn is_pattern(&self) -> bool {
        matches!(self, Self::Pattern(_))
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel(name) => f.write_str(name),
            Self::Pattern(name) => write!(f, "{name} (pattern)"),
        }
    }
}

/// Kind of a [`Target`], as picked in the add-subscription popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Channel,
    Pattern,
}

impl TargetKind {
    #[must_use]
    pub fn flipped(self) -> Self {
        match self {
            Self::Channel => Self::Pattern,
            Self::Pattern => Self::Channel,
        }
    }
}

/// Glob characters (`*`, `?`, `[`) make the text a pattern by default.
pub fn detect_kind(text: &str) -> TargetKind {
    if text.contains(['*', '?', '[']) {
        TargetKind::Pattern
    } else {
        TargetKind::Channel
    }
}

/// A stored message payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    /// Valid or mostly printable UTF-8 (decoded lossily).
    Text(String),
    /// Mostly non-printable bytes.
    Binary(Vec<u8>),
}

impl Payload {
    /// Decode `bytes`, keeping at most `max` bytes. Returns the payload and
    /// whether it was cut.
    pub fn from_bytes(bytes: &[u8], max: usize) -> (Self, bool) {
        let truncated = bytes.len() > max;
        if let Ok(text) = std::str::from_utf8(bytes) {
            let end = text.floor_char_boundary(max);
            return (Self::Text(text[..end].to_owned()), truncated);
        }
        let cut = &bytes[..bytes.len().min(max)];
        if looks_binary(bytes) {
            (Self::Binary(cut.to_vec()), truncated)
        } else {
            (
                Self::Text(String::from_utf8_lossy(cut).into_owned()),
                truncated,
            )
        }
    }

    /// One-line preview: control characters become spaces; binary payloads
    /// show their original `size` and the first bytes in hex.
    pub fn preview(&self, size: usize) -> String {
        match self {
            Self::Text(text) => text
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
            Self::Binary(bytes) => {
                let mut out = format!("[binary {size} B]");
                for b in bytes.iter().take(HEX_PREVIEW) {
                    let _ = write!(out, " {b:02x}");
                }
                if bytes.len() > HEX_PREVIEW {
                    out.push_str(" …");
                }
                out
            }
        }
    }

    /// What `y` copies: the stored text, or the stored bytes as
    /// space-separated hex (the clipboard takes text only).
    pub fn copy_text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Binary(bytes) => bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// `00000000  0a 1f … 8c  |..ascii..|` rows, 16 bytes each. A text
    /// payload is dumped as its UTF-8 bytes.
    pub fn hex_dump(&self) -> Vec<String> {
        let bytes = match self {
            Self::Text(text) => text.as_bytes(),
            Self::Binary(bytes) => bytes,
        };
        bytes
            .chunks(HEX_ROW)
            .enumerate()
            .map(|(row, chunk)| {
                let mut out = format!("{:08x} ", row * HEX_ROW);
                for i in 0..HEX_ROW {
                    if i % 8 == 0 {
                        out.push(' ');
                    }
                    match chunk.get(i) {
                        Some(b) => {
                            let _ = write!(out, "{b:02x} ");
                        }
                        None => out.push_str("   "),
                    }
                }
                out.push('|');
                out.extend(chunk.iter().map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        char::from(b)
                    } else {
                        '.'
                    }
                }));
                out.push('|');
                out
            })
            .collect()
    }

    /// Stored size in bytes.
    pub fn len(&self) -> usize {
        match self {
            Self::Text(text) => text.len(),
            Self::Binary(bytes) => bytes.len(),
        }
    }
}

/// More than 10 % of the first [`BINARY_SNIFF`] bytes are control bytes.
fn looks_binary(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(BINARY_SNIFF)];
    let odd = head
        .iter()
        .filter(|&&b| (b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r')) || b == 0x7f)
        .count();
    odd * 10 > head.len()
}

/// What a log entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A message received from Redis.
    Message,
    /// The connection came back after an outage; messages published
    /// meanwhile are lost. Empty payload, not counted in stats.
    Reconnected,
}

/// One received message (or a marker row, see [`EntryKind`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubMessage {
    pub kind: EntryKind,
    /// Increases by one per message of a subscription, across evictions and
    /// clears.
    pub seq: u64,
    pub received_at: SystemTime,
    /// The concrete channel (differs from the pattern for `PSUBSCRIBE`).
    pub channel: String,
    pub payload: Payload,
    /// Original payload size in bytes.
    pub size: usize,
    /// The payload was cut at `max_payload`.
    pub truncated: bool,
}

/// Buffer limits of every subscription (from `PubSubConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferLimits {
    pub messages: usize,
    pub bytes: usize,
    /// Most payload bytes stored per message; `<= bytes`.
    pub payload: usize,
}

/// Newest messages of a subscription, bounded by count and bytes.
#[derive(Debug, Clone)]
pub struct MessageBuffer {
    items: VecDeque<PubSubMessage>,
    /// Sum of stored payload sizes.
    bytes: usize,
    limits: BufferLimits,
    next_seq: u64,
}

impl MessageBuffer {
    pub fn new(limits: BufferLimits) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            limits,
            next_seq: 1,
        }
    }

    /// Store a message (payload cut at `max_payload`), then evict the oldest
    /// ones until both limits hold.
    pub fn push(&mut self, channel: &str, payload: &[u8], now: SystemTime) {
        let (stored, truncated) = Payload::from_bytes(payload, self.limits.payload);
        self.bytes += stored.len();
        self.items.push_back(PubSubMessage {
            kind: EntryKind::Message,
            seq: self.next_seq,
            received_at: now,
            channel: channel.to_owned(),
            payload: stored,
            size: payload.len(),
            truncated,
        });
        self.next_seq += 1;
        self.evict();
    }

    /// Store a [`EntryKind::Reconnected`] marker. It takes a `seq` like a
    /// message, so views keyed by sequence number stay consistent.
    pub fn push_marker(&mut self, now: SystemTime) {
        self.items.push_back(PubSubMessage {
            kind: EntryKind::Reconnected,
            seq: self.next_seq,
            received_at: now,
            channel: String::new(),
            payload: Payload::Text(String::new()),
            size: 0,
            truncated: false,
        });
        self.next_seq += 1;
        self.evict();
    }

    /// Drop the oldest entries until both limits hold.
    fn evict(&mut self) {
        while self.items.len() > self.limits.messages || self.bytes > self.limits.bytes {
            let Some(old) = self.items.pop_front() else {
                break;
            };
            self.bytes -= old.payload.len();
        }
    }

    /// Drop every message; sequence numbers keep counting.
    pub fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// The newest `n` messages, oldest first.
    pub fn last_n(&self, n: usize) -> impl DoubleEndedIterator<Item = &PubSubMessage> {
        self.items.iter().skip(self.items.len().saturating_sub(n))
    }

    /// Up to `n` entries from index `start`, oldest first.
    pub fn range(&self, start: usize, n: usize) -> impl Iterator<Item = &PubSubMessage> {
        self.items.iter().skip(start).take(n)
    }

    pub fn get(&self, index: usize) -> Option<&PubSubMessage> {
        self.items.get(index)
    }

    /// Sequence number of the oldest entry still kept.
    pub fn first_seq(&self) -> Option<u64> {
        self.items.front().map(|m| m.seq)
    }

    /// Sequence number of the newest entry.
    pub fn last_seq(&self) -> Option<u64> {
        self.items.back().map(|m| m.seq)
    }

    /// Index of the entry with `seq`, or where it would go. Sequence numbers
    /// only increase, so the buffer is sorted by them.
    pub fn index_of(&self, seq: u64) -> Result<usize, usize> {
        self.items.binary_search_by_key(&seq, |m| m.seq)
    }
}

/// Counters of a subscription. Rates come from a ring of per-second counts.
#[derive(Debug, Clone)]
pub struct SubStats {
    pub total: u64,
    pub bytes: u64,
    pub max_size: usize,
    /// Per-second counts, indexed by unix second % 60.
    buckets: [u32; RATE_WINDOW],
    /// Unix second of the newest bucket written.
    head: u64,
    pub last_at: Option<SystemTime>,
    /// Messages per concrete channel (pattern subscriptions only), at most
    /// [`PER_CHANNEL_CAP`] names.
    pub per_channel: HashMap<String, u64>,
    /// Messages of channels past the cap.
    pub other: u64,
}

impl Default for SubStats {
    fn default() -> Self {
        Self {
            total: 0,
            bytes: 0,
            max_size: 0,
            buckets: [0; RATE_WINDOW],
            head: 0,
            last_at: None,
            per_channel: HashMap::new(),
            other: 0,
        }
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[allow(clippy::cast_possible_truncation)]
fn slot(sec: u64) -> usize {
    (sec % RATE_WINDOW as u64) as usize
}

impl SubStats {
    /// Count a message of `size` bytes. `channel` is its concrete channel,
    /// set for pattern subscriptions only.
    pub fn record(&mut self, size: usize, now: SystemTime, channel: Option<&str>) {
        if let Some(channel) = channel {
            if let Some(count) = self.per_channel.get_mut(channel) {
                *count += 1;
            } else if self.per_channel.len() < PER_CHANNEL_CAP {
                self.per_channel.insert(channel.to_owned(), 1);
            } else {
                self.other += 1;
            }
        }
        self.total += 1;
        self.bytes += size as u64;
        self.max_size = self.max_size.max(size);
        self.last_at = Some(now);

        let sec = unix_secs(now);
        if sec > self.head {
            // Zero the buckets of the seconds skipped since the last message.
            let gap = (sec - self.head).min(RATE_WINDOW as u64);
            for s in (sec + 1 - gap)..=sec {
                self.buckets[slot(s)] = 0;
            }
            self.head = sec;
        }
        // A clock that went back counts into the newest bucket.
        let i = slot(self.head);
        self.buckets[i] = self.buckets[i].saturating_add(1);
    }

    /// Count of second `sec`, or 0 if it's outside the ring.
    fn count_at(&self, sec: u64) -> u32 {
        if sec > self.head || self.head - sec >= RATE_WINDOW as u64 {
            0
        } else {
            self.buckets[slot(sec)]
        }
    }

    /// Messages in the last complete second.
    pub fn rate_1s(&self, now: SystemTime) -> u32 {
        unix_secs(now)
            .checked_sub(1)
            .map_or(0, |sec| self.count_at(sec))
    }

    /// Average msg/s over the last 60 complete seconds.
    pub fn rate_60s(&self, now: SystemTime) -> f64 {
        let sec = unix_secs(now);
        let sum: u32 = (1..=RATE_WINDOW as u64)
            .filter_map(|back| sec.checked_sub(back))
            .map(|s| self.count_at(s))
            .sum();
        f64::from(sum) / f64::from(RATE_WINDOW_SECS)
    }

    /// Average message size in bytes (0 before the first message).
    pub fn avg_size(&self) -> u64 {
        self.bytes.checked_div(self.total).unwrap_or(0)
    }

    /// The `n` busiest concrete channels, busiest first (ties by name).
    pub fn top_channels(&self, n: usize) -> Vec<(&str, u64)> {
        let mut all: Vec<(&str, u64)> = self
            .per_channel
            .iter()
            .map(|(name, count)| (name.as_str(), *count))
            .collect();
        all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        all.truncate(n);
        all
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Characters of a payload kept in a notification preview (the toast cuts
/// it further to its width).
const PREVIEW_CHARS: usize = 200;

/// One subscription and what it has received.
#[derive(Debug, Clone)]
pub struct Subscription {
    pub target: Target,
    pub buffer: MessageBuffer,
    pub stats: SubStats,
    /// Notifications for this subscription (off by default).
    pub notify: NotifyState,
    /// Messages received while the subscription wasn't open in the tab.
    pub unread: u64,
    /// `stats.total` at the last notifier tick: the difference is what
    /// arrived since (evictions and markers don't skew it).
    pub seen_total: u64,
}

impl Subscription {
    fn new(target: Target, limits: BufferLimits) -> Self {
        Self {
            target,
            buffer: MessageBuffer::new(limits),
            stats: SubStats::default(),
            notify: NotifyState::Off,
            unread: 0,
            seen_total: 0,
        }
    }

    /// `channel: payload` of the newest message, for a notification.
    pub fn newest_preview(&self) -> Option<String> {
        let msg = self
            .buffer
            .last_n(usize::MAX)
            .rev()
            .find(|m| m.kind == EntryKind::Message)?;
        let payload: String = msg
            .payload
            .preview(msg.size)
            .chars()
            .take(PREVIEW_CHARS)
            .collect();
        Some(format!("{}: {payload}", msg.channel))
    }

    fn matches(&self, channel: &str, pattern: Option<&str>) -> bool {
        match (&self.target, pattern) {
            (Target::Pattern(p), Some(pattern)) => p == pattern,
            (Target::Channel(c), None) => c == channel,
            _ => false,
        }
    }
}

/// State of the dedicated Pub/Sub connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnStatus {
    /// Nothing subscribed yet: no connection opened.
    #[default]
    NotStarted,
    Connecting,
    Connected,
    /// The connection is down; attempt number `attempt` starts at `next_at`.
    Reconnecting {
        attempt: u32,
        next_at: Instant,
    },
}

impl ConnStatus {
    /// Header text: `connected`, `reconnecting in 4 s`, …
    #[must_use]
    pub fn label(&self, now: Instant) -> String {
        match self {
            Self::NotStarted => "not started".into(),
            Self::Connecting => "connecting".into(),
            Self::Connected => "connected".into(),
            Self::Reconnecting { next_at, .. } => {
                let left = next_at.saturating_duration_since(now);
                if left.is_zero() {
                    "reconnecting…".into()
                } else {
                    // Round up: "in 0 s" would read as "now".
                    let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
                    format!("reconnecting in {secs} s")
                }
            }
        }
    }
}

/// Result of `PUBSUB CHANNELS` / `NUMSUB` / `NUMPAT`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerChannels {
    /// Name and subscriber count, sorted by name, at most the listing cap.
    pub channels: Vec<(String, u64)>,
    /// Channels the server reported before the cap.
    pub total: usize,
    /// `PUBSUB NUMPAT`.
    pub patterns: u64,
}

/// Everything the Pub/Sub tab shows, shared between the task and the UI.
#[derive(Debug, Clone)]
pub struct PubSubStore {
    /// In the order they were added.
    pub subs: Vec<Subscription>,
    pub status: ConnStatus,
    /// Last `PUBSUB CHANNELS` listing; `None` until the first one arrives.
    pub server: Option<ServerChannels>,
    limits: BufferLimits,
}

impl PubSubStore {
    #[must_use]
    pub fn new(limits: BufferLimits) -> Self {
        Self {
            subs: Vec::new(),
            status: ConnStatus::NotStarted,
            server: None,
            limits,
        }
    }

    #[must_use]
    pub fn position(&self, target: &Target) -> Option<usize> {
        self.subs.iter().position(|s| &s.target == target)
    }

    /// Add a subscription. Returns its index and whether it is new (a
    /// duplicate returns the existing one).
    pub fn add(&mut self, target: Target) -> (usize, bool) {
        if let Some(index) = self.position(&target) {
            return (index, false);
        }
        self.subs.push(Subscription::new(target, self.limits));
        (self.subs.len() - 1, true)
    }

    /// Remove a subscription with its buffer and stats.
    pub fn remove(&mut self, target: &Target) -> bool {
        let before = self.subs.len();
        self.subs.retain(|s| &s.target != target);
        self.subs.len() != before
    }

    /// Clear the buffer and stats of subscription `index`.
    pub fn clear(&mut self, index: usize) {
        if let Some(sub) = self.subs.get_mut(index) {
            sub.buffer.clear();
            sub.stats.reset();
            sub.seen_total = 0;
            sub.unread = 0;
        }
    }

    /// Every target, in the order added: what a (re)connect subscribes.
    #[must_use]
    pub fn targets(&self) -> Vec<Target> {
        self.subs.iter().map(|s| s.target.clone()).collect()
    }

    pub fn set_server_channels(&mut self, server: ServerChannels) {
        self.server = Some(server);
    }

    /// Server channels without an exact subscription of ours: those are
    /// listed under Subscriptions only. A pattern that matches doesn't hide
    /// a channel.
    pub fn server_only_channels(&self) -> impl Iterator<Item = &(String, u64)> {
        self.server
            .iter()
            .flat_map(|s| s.channels.iter())
            .filter(|(name, _)| {
                !self
                    .subs
                    .iter()
                    .any(|sub| matches!(&sub.target, Target::Channel(c) if c == name))
            })
    }

    /// Add a reconnect marker to each of `targets` still subscribed.
    pub fn mark_reconnected(&mut self, targets: &[Target], now: SystemTime) {
        for sub in &mut self.subs {
            if targets.contains(&sub.target) {
                sub.buffer.push_marker(now);
            }
        }
    }

    /// Store a message: `pattern` set means it came from `PSUBSCRIBE pattern`.
    /// Messages for a target that was removed meanwhile are dropped.
    pub fn push(&mut self, channel: &str, pattern: Option<&str>, payload: &[u8], now: SystemTime) {
        if let Some(sub) = self.subs.iter_mut().find(|s| s.matches(channel, pattern)) {
            sub.buffer.push(channel, payload, now);
            let concrete = sub.target.is_pattern().then_some(channel);
            sub.stats.record(payload.len(), now, concrete);
        }
    }

    /// Store a message received from Redis.
    pub fn push_msg(&mut self, msg: &redis::Msg, now: SystemTime) {
        let pattern: Option<String> = msg.get_pattern().ok().flatten();
        self.push(
            msg.get_channel_name(),
            pattern.as_deref(),
            msg.get_payload_bytes(),
            now,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use redis::Value;

    use super::*;

    const LIMITS: BufferLimits = BufferLimits {
        messages: 3,
        bytes: 1024,
        payload: 512,
    };

    fn at(sec: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + sec)
    }

    fn seqs(buf: &MessageBuffer) -> Vec<u64> {
        buf.last_n(usize::MAX).map(|m| m.seq).collect()
    }

    #[test]
    fn evicts_by_count() {
        let mut buf = MessageBuffer::new(LIMITS);
        for i in 0..5 {
            buf.push("c", format!("m{i}").as_bytes(), at(0));
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(seqs(&buf), vec![3, 4, 5]);
        assert_eq!(buf.bytes(), 6);
    }

    #[test]
    fn evicts_by_bytes() {
        let mut buf = MessageBuffer::new(BufferLimits {
            messages: 100,
            bytes: 10,
            payload: 10,
        });
        buf.push("c", b"aaaa", at(0));
        buf.push("c", b"bbbb", at(0));
        assert_eq!(buf.len(), 2);
        buf.push("c", b"cccc", at(0));
        assert_eq!(buf.len(), 2);
        assert_eq!(buf.bytes(), 8);
        assert_eq!(seqs(&buf), vec![2, 3]);
    }

    #[test]
    fn cuts_payload_at_max_payload() {
        let mut buf = MessageBuffer::new(BufferLimits {
            messages: 10,
            bytes: 100,
            payload: 4,
        });
        buf.push("c", b"abcdefgh", at(0));
        buf.push("c", b"abc", at(0));
        let msgs: Vec<_> = buf.last_n(2).collect();
        assert_eq!(msgs[0].payload, Payload::Text("abcd".into()));
        assert_eq!(msgs[0].size, 8);
        assert!(msgs[0].truncated);
        assert_eq!(msgs[1].payload, Payload::Text("abc".into()));
        assert!(!msgs[1].truncated);
        assert_eq!(buf.bytes(), 7);
    }

    #[test]
    fn cut_keeps_utf8_char_boundaries() {
        // "é" is two bytes: cutting at 3 must not split the second one.
        let (payload, truncated) = Payload::from_bytes("aéé".as_bytes(), 4);
        assert_eq!(payload, Payload::Text("aé".into()));
        assert!(truncated);
    }

    #[test]
    fn binary_detection() {
        let (p, _) = Payload::from_bytes(&[0xff, 0x00, 0x01, 0x02, 0x8c], 64);
        assert_eq!(p, Payload::Binary(vec![0xff, 0x00, 0x01, 0x02, 0x8c]));
        // Invalid UTF-8 but mostly printable: lossy text.
        let (p, _) = Payload::from_bytes(b"hello world\xff", 64);
        assert_eq!(p, Payload::Text("hello world\u{fffd}".into()));
        // Valid UTF-8 is always text.
        let (p, _) = Payload::from_bytes(b"\x00\x01\x02", 64);
        assert!(matches!(p, Payload::Text(_)));
    }

    #[test]
    fn preview_is_one_line() {
        let text = Payload::Text("a\nb\tc".into());
        assert_eq!(text.preview(5), "a b c");
        let bin = Payload::Binary((0u8..20).collect());
        assert_eq!(
            bin.preview(312),
            "[binary 312 B] 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f …"
        );
    }

    #[test]
    fn seq_keeps_increasing_across_evictions_and_clear() {
        let mut buf = MessageBuffer::new(LIMITS);
        for _ in 0..4 {
            buf.push("c", b"x", at(0));
        }
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.bytes(), 0);
        buf.push("c", b"x", at(0));
        assert_eq!(seqs(&buf), vec![5]);
    }

    #[test]
    fn rate_buckets_over_synthetic_seconds() {
        let mut stats = SubStats::default();
        for _ in 0..5 {
            stats.record(10, at(0), None);
        }
        for _ in 0..3 {
            stats.record(20, at(1), None);
        }
        assert_eq!(stats.rate_1s(at(1)), 5, "second 1 isn't complete yet");
        assert_eq!(stats.rate_1s(at(2)), 3);
        assert_eq!(stats.rate_1s(at(3)), 0);
        assert!((stats.rate_60s(at(2)) - 8.0 / 60.0).abs() < 1e-9);
        assert_eq!(stats.total, 8);
        assert_eq!(stats.bytes, 110);
        assert_eq!(stats.max_size, 20);
        assert_eq!(stats.last_at, Some(at(1)));

        // 60 s later the old seconds are out of the window.
        assert!(stats.rate_60s(at(62)).abs() < 1e-9);

        // A gap zeroes stale buckets that share a slot with new seconds.
        stats.record(1, at(60), None);
        assert_eq!(stats.rate_1s(at(61)), 1);
        assert_eq!(stats.count_at(0), 0);
        assert!((stats.rate_60s(at(61)) - 4.0 / 60.0).abs() < 1e-9);

        stats.reset();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.rate_1s(at(61)), 0);
    }

    #[test]
    fn detects_pattern_vs_channel() {
        assert_eq!(detect_kind("orders:new"), TargetKind::Channel);
        assert_eq!(detect_kind("orders:*"), TargetKind::Pattern);
        assert_eq!(detect_kind("h?llo"), TargetKind::Pattern);
        assert_eq!(detect_kind("h[ae]llo"), TargetKind::Pattern);
        assert_eq!(detect_kind(""), TargetKind::Channel);
        assert_eq!(TargetKind::Channel.flipped(), TargetKind::Pattern);
    }

    #[test]
    fn duplicate_add_returns_existing() {
        let mut store = PubSubStore::new(LIMITS);
        assert_eq!(store.add(Target::Channel("a".into())), (0, true));
        assert_eq!(store.add(Target::Pattern("a".into())), (1, true));
        assert_eq!(store.add(Target::Channel("a".into())), (0, false));
        assert!(store.remove(&Target::Channel("a".into())));
        assert!(!store.remove(&Target::Channel("a".into())));
        assert_eq!(store.targets(), vec![Target::Pattern("a".into())]);
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    #[test]
    fn pmessage_keeps_the_concrete_channel() {
        let mut store = PubSubStore::new(LIMITS);
        store.add(Target::Pattern("orders:*".into()));
        store.add(Target::Channel("orders:new".into()));

        let pmessage = redis::Msg::from_owned_value(Value::Array(vec![
            bulk("pmessage"),
            bulk("orders:*"),
            bulk("orders:new"),
            bulk(r#"{"id":1}"#),
        ]))
        .unwrap();
        let message = redis::Msg::from_owned_value(Value::Array(vec![
            bulk("message"),
            bulk("orders:new"),
            bulk(r#"{"id":1}"#),
        ]))
        .unwrap();
        store.push_msg(&pmessage, at(0));
        store.push_msg(&message, at(0));

        // Each subscription gets its own copy.
        for sub in &store.subs {
            let msgs: Vec<_> = sub.buffer.last_n(10).collect();
            assert_eq!(msgs.len(), 1, "{}", sub.target);
            assert_eq!(msgs[0].channel, "orders:new");
            assert_eq!(msgs[0].payload, Payload::Text(r#"{"id":1}"#.into()));
        }
    }

    #[test]
    fn server_only_channels_hide_own_exact_subscriptions() {
        let mut store = PubSubStore::new(LIMITS);
        assert_eq!(store.server_only_channels().count(), 0);
        store.add(Target::Channel("chat".into()));
        store.add(Target::Pattern("events".into()));
        store.set_server_channels(ServerChannels {
            channels: vec![("chat".into(), 2), ("events".into(), 1), ("jobs".into(), 1)],
            total: 3,
            patterns: 1,
        });
        let names: Vec<_> = store
            .server_only_channels()
            .map(|(n, _)| n.as_str())
            .collect();
        // A pattern with the same name doesn't hide the channel.
        assert_eq!(names, vec!["events", "jobs"]);
    }

    #[test]
    fn resubscribe_list_follows_changes_made_while_down() {
        let mut store = PubSubStore::new(LIMITS);
        store.add(Target::Pattern("orders:*".into()));
        store.add(Target::Channel("chat".into()));
        let lost = store.targets();
        // While reconnecting the user removes one and adds another.
        store.remove(&Target::Channel("chat".into()));
        store.add(Target::Channel("jobs".into()));
        store.add(Target::Pattern("orders:*".into()));
        assert_eq!(
            store.targets(),
            vec![
                Target::Pattern("orders:*".into()),
                Target::Channel("jobs".into())
            ]
        );

        // Only subscriptions held before the outage get a marker.
        store.push("orders:new", Some("orders:*"), b"x", at(0));
        store.mark_reconnected(&lost, at(1));
        let entries: Vec<_> = store.subs[0].buffer.last_n(10).collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].kind, EntryKind::Reconnected);
        assert_eq!(entries[1].seq, 2);
        assert!(store.subs[1].buffer.is_empty());
        // Markers aren't messages.
        assert_eq!(store.subs[0].stats.total, 1);
        assert_eq!(store.subs[0].buffer.bytes(), 1);
    }

    #[test]
    fn status_labels() {
        let now = Instant::now();
        assert_eq!(ConnStatus::NotStarted.label(now), "not started");
        assert_eq!(ConnStatus::Connected.label(now), "connected");
        let status = ConnStatus::Reconnecting {
            attempt: 3,
            next_at: now + Duration::from_millis(3_200),
        };
        assert_eq!(status.label(now), "reconnecting in 4 s");
        assert_eq!(
            status.label(now + Duration::from_secs(3)),
            "reconnecting in 1 s"
        );
        assert_eq!(status.label(now + Duration::from_secs(5)), "reconnecting…");
    }

    #[test]
    fn clear_resets_buffer_and_stats() {
        let mut store = PubSubStore::new(LIMITS);
        store.add(Target::Channel("c".into()));
        store.push("c", None, b"x", at(0));
        store.push("other", None, b"x", at(0));
        assert_eq!(store.subs[0].stats.total, 1);
        store.clear(0);
        assert!(store.subs[0].buffer.is_empty());
        assert_eq!(store.subs[0].stats.total, 0);
    }

    #[test]
    fn clear_resets_notifier_counters() {
        let mut store = PubSubStore::new(LIMITS);
        store.add(Target::Channel("c".into()));
        store.push("c", None, b"x", at(0));
        store.subs[0].seen_total = 1;
        store.subs[0].unread = 1;
        store.clear(0);
        assert_eq!(store.subs[0].seen_total, 0);
        assert_eq!(store.subs[0].unread, 0);
    }

    #[test]
    fn newest_preview_skips_markers() {
        let mut store = PubSubStore::new(LIMITS);
        store.add(Target::Pattern("orders:*".into()));
        assert_eq!(store.subs[0].newest_preview(), None);
        store.push("orders:new", Some("orders:*"), b"{\"id\":7}", at(0));
        store.mark_reconnected(&store.targets(), at(1));
        assert_eq!(
            store.subs[0].newest_preview().as_deref(),
            Some("orders:new: {\"id\":7}")
        );
    }

    #[test]
    fn per_channel_counts_patterns_only_up_to_the_cap() {
        let mut store = PubSubStore::new(LIMITS);
        store.add(Target::Pattern("orders:*".into()));
        store.add(Target::Channel("orders:new".into()));
        store.push("orders:new", Some("orders:*"), b"x", at(0));
        store.push("orders:new", Some("orders:*"), b"x", at(0));
        store.push("orders:paid", Some("orders:*"), b"x", at(0));
        store.push("orders:new", None, b"x", at(0));
        let pattern = &store.subs[0].stats;
        assert_eq!(
            pattern.top_channels(5),
            vec![("orders:new", 2), ("orders:paid", 1)]
        );
        assert!(store.subs[1].stats.per_channel.is_empty(), "channel subs");

        let mut stats = SubStats::default();
        for i in 0..PER_CHANNEL_CAP {
            stats.record(1, at(0), Some(&format!("c{i}")));
        }
        stats.record(1, at(0), Some("late"));
        stats.record(1, at(0), Some("late"));
        stats.record(1, at(0), Some("c7"));
        assert_eq!(stats.per_channel.len(), PER_CHANNEL_CAP);
        assert_eq!(stats.other, 2);
        assert_eq!(stats.per_channel["c7"], 2);
        assert_eq!(stats.top_channels(2), vec![("c7", 2), ("c0", 1)]);

        stats.reset();
        assert!(stats.per_channel.is_empty());
        assert_eq!(stats.other, 0);
    }

    #[test]
    fn average_size() {
        let mut stats = SubStats::default();
        assert_eq!(stats.avg_size(), 0);
        stats.record(100, at(0), None);
        stats.record(300, at(0), None);
        stats.record(9, at(0), None);
        assert_eq!(stats.avg_size(), 136);
        assert_eq!(stats.max_size, 300);
    }

    /// `odd` control bytes in an invalid-UTF-8 payload of `len` bytes.
    fn sample(len: usize, odd: usize) -> Vec<u8> {
        let mut bytes = vec![b'a'; len];
        bytes[0] = 0xff;
        for b in bytes.iter_mut().skip(1).take(odd) {
            *b = 0x01;
        }
        bytes
    }

    #[test]
    fn binary_detection_thresholds() {
        let kind = |bytes: &[u8]| matches!(Payload::from_bytes(bytes, 1024).0, Payload::Binary(_));
        // Exactly 10 % is still text; more is binary.
        assert!(!kind(&sample(100, 10)));
        assert!(kind(&sample(100, 11)));
        // Only the first 256 bytes count: 25 of them is under 10 %, whatever
        // follows.
        let mut long = sample(256, 25);
        long.extend(std::iter::repeat_n(0x01, 500));
        assert!(!kind(&long));
        assert!(kind(&sample(256, 26)));
        // Tabs and newlines are printable.
        let mut text = sample(100, 0);
        text[1..30].fill(b'\n');
        assert!(!kind(&text));
    }

    #[test]
    fn hex_dump_and_copy_text() {
        let bin = Payload::Binary((0u8..20).chain(*b"A ").collect());
        assert_eq!(
            bin.hex_dump(),
            vec![
                "00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f |................|",
                "00000010  10 11 12 13 41 20                                |....A |",
            ]
        );
        assert!(bin.copy_text().starts_with("00 01 02"));
        assert_eq!(Payload::Text("hi".into()).copy_text(), "hi");
    }

    #[test]
    fn index_of_follows_seq() {
        let mut buf = MessageBuffer::new(LIMITS);
        assert_eq!(buf.first_seq(), None);
        for _ in 0..5 {
            buf.push("c", b"x", at(0));
        }
        assert_eq!((buf.first_seq(), buf.last_seq()), (Some(3), Some(5)));
        assert_eq!(buf.index_of(4), Ok(1));
        assert_eq!(buf.index_of(1), Err(0));
        assert_eq!(buf.get(2).map(|m| m.seq), Some(5));
        let seqs: Vec<u64> = buf.range(1, 5).map(|m| m.seq).collect();
        assert_eq!(seqs, vec![4, 5]);
    }
}
