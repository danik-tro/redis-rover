//! Background task owning the dedicated MONITOR connection.
//!
//! It idles until [`MonitorCmd::Start`], so a session that never opens the
//! screen costs the server nothing. Lines go straight into the
//! [`MonitorStore`] in batches; there is no `Action` per line. Dropping the
//! connection is what ends MONITOR on the server.

use std::{
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant as StdInstant, SystemTime, UNIX_EPOCH},
};

use futures::{FutureExt, Stream, StreamExt};
use parking_lot::Mutex;
use redis::Value;
use tokio::{sync::mpsc::UnboundedReceiver, time::Instant};
use tokio_util::sync::CancellationToken;

use super::{
    conn::client_at,
    monitor_store::{parse_line, MonitorStatus, MonitorStore, ParsedLine},
    pubsub::{backoff, is_connection_error},
    sentinel::ServerClient,
};

/// How long opening the connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Most lines read before the lock is taken and commands are looked at.
const MAX_BATCH: usize = 512;
/// How long the check line may take to come back on the MONITOR stream.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
/// Shown when MONITOR never reported the check line.
const REFUSED: &str = "the server did not start MONITOR (refused by an ACL, or disabled)";
/// How often the sampling rate is worked out again.
const SAMPLE_WINDOW: Duration = Duration::from_millis(250);

/// Control messages from the App.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorCmd {
    Start,
    Stop,
}

/// Keeps one line in `n`, with `n` chosen so the kept lines stay at or under
/// `max_rate` per second.
#[derive(Debug)]
struct Sampler {
    max_rate: u64,
    every: u32,
    /// Lines since the last kept one.
    skipped: u32,
    window_start: Option<StdInstant>,
    window_seen: u64,
}

impl Sampler {
    fn new(max_rate: u64) -> Self {
        Self {
            max_rate: max_rate.max(1),
            every: 1,
            skipped: 0,
            window_start: None,
            window_seen: 0,
        }
    }

    /// Whether to keep the next line.
    fn keep(&mut self) -> bool {
        self.window_seen += 1;
        self.skipped += 1;
        if self.skipped >= self.every {
            self.skipped = 0;
            true
        } else {
            false
        }
    }

    /// At the end of a window, set `every` from the rate it saw.
    fn roll(&mut self, now: StdInstant) {
        let Some(start) = self.window_start else {
            self.window_start = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(start);
        if elapsed < SAMPLE_WINDOW {
            return;
        }
        let per_sec = u128::from(self.window_seen) * 1000 / elapsed.as_millis().max(1);
        let every = per_sec.div_ceil(u128::from(self.max_rate)).max(1);
        self.every = u32::try_from(every).unwrap_or(u32::MAX);
        self.window_start = Some(now);
        self.window_seen = 0;
    }

    fn reset(&mut self) {
        *self = Self::new(self.max_rate);
    }
}

type Lines = Pin<Box<dyn Stream<Item = Value> + Send>>;

/// Run the MONITOR task until `cancel` fires or the command channel closes.
pub async fn run(
    client: impl Into<ServerClient>,
    store: Arc<Mutex<MonitorStore>>,
    node: Arc<Mutex<Option<(String, u16)>>>,
    max_rate: u64,
    mut cmd_rx: UnboundedReceiver<MonitorCmd>,
    cancel: CancellationToken,
) {
    let mut task = MonitorTask {
        client: client.into(),
        store,
        node,
        sampler: Sampler::new(max_rate),
        conn: None,
        retry: None,
    };
    loop {
        let retry_at = task.retry.map(|(_, at)| at);
        // Biased: a flood of lines must never delay a stop.
        tokio::select! {
            biased;
            () = cancel.cancelled() => break,
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => task.apply(cmd).await,
                None => break,
            },
            () = sleep_until(retry_at) => task.reconnect().await,
            () = task.client.changed() => task.lost(),
            value = next_value(task.conn.as_mut()) => match value {
                Some(value) => task.ingest(&value),
                None => task.lost(),
            },
        }
    }
}

async fn next_value(conn: Option<&mut Lines>) -> Option<Value> {
    match conn {
        Some(lines) => lines.next().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct MonitorTask {
    /// The cluster node to watch (`SharedState::ops_node`).
    node: Arc<Mutex<Option<(String, u16)>>>,
    /// The server, or behind Sentinel whichever is the master now.
    client: ServerClient,
    store: Arc<Mutex<MonitorStore>>,
    sampler: Sampler,
    conn: Option<Lines>,
    /// Pending reconnect: attempt number and when it starts.
    retry: Option<(u32, Instant)>,
}

impl MonitorTask {
    async fn apply(&mut self, cmd: MonitorCmd) {
        match cmd {
            MonitorCmd::Start => {
                if self.conn.is_some() || self.retry.is_some() {
                    return;
                }
                self.sampler.reset();
                self.connect(1).await;
            }
            MonitorCmd::Stop => {
                self.conn = None;
                self.retry = None;
                self.store.lock().set_status(MonitorStatus::Stopped);
            }
        }
    }

    /// Open the connection. A refusal is final; a network failure retries.
    async fn connect(&mut self, attempt: u32) {
        self.store.lock().set_status(MonitorStatus::Connecting);
        // On a cluster, the node picked on the screen.
        let picked = self.node.lock().clone();
        let base = self.client.current();
        let client = match picked {
            Some((host, port)) => match client_at(&base, &host, port) {
                Ok(client) => client,
                Err(err) => {
                    self.store
                        .lock()
                        .set_status(MonitorStatus::Failed(err.to_string()));
                    return;
                }
            },
            None => base,
        };
        let opened = tokio::time::timeout(CONNECT_TIMEOUT, client.get_async_monitor()).await;
        match opened {
            Ok(Ok(monitor)) => {
                // `Value` always converts: another target type would end
                // the stream on the first line it can't read.
                let mut lines: Lines = Box::pin(monitor.into_on_message::<Value>());
                match confirm_started(&client, &mut lines).await {
                    Confirmed::Yes => {
                        self.conn = Some(lines);
                        self.store.lock().set_status(MonitorStatus::Running);
                    }
                    Confirmed::Refused => {
                        log::warn!("MONITOR refused");
                        self.store
                            .lock()
                            .set_status(MonitorStatus::Failed(REFUSED.to_owned()));
                    }
                    Confirmed::Lost => self.schedule(attempt),
                }
            }
            Ok(Err(err)) if !is_connection_error(&err) => {
                log::warn!("MONITOR refused: {err}");
                self.store
                    .lock()
                    .set_status(MonitorStatus::Failed(err.to_string()));
            }
            Ok(Err(err)) => {
                log::warn!("MONITOR connection failed: {err}");
                self.schedule(attempt);
            }
            Err(_) => {
                log::warn!("MONITOR connection timed out after {CONNECT_TIMEOUT:?}");
                self.schedule(attempt);
            }
        }
    }

    async fn reconnect(&mut self) {
        let Some((attempt, _)) = self.retry.take() else {
            return;
        };
        self.connect(attempt.saturating_add(1)).await;
    }

    fn schedule(&mut self, attempt: u32) {
        let next_at = Instant::now() + backoff(attempt);
        self.retry = Some((attempt, next_at));
        self.store.lock().set_status(MonitorStatus::Reconnecting {
            attempt,
            next_at: next_at.into_std(),
        });
    }

    fn lost(&mut self) {
        if self.conn.take().is_some() {
            log::warn!("MONITOR connection lost");
            self.schedule(1);
        }
    }

    /// Take `first` and whatever else is already buffered, parse outside the
    /// lock, and store it all in one short lock.
    fn ingest(&mut self, first: &Value) {
        self.sampler.roll(StdInstant::now());
        let mut kept = Vec::new();
        let mut seen = 1;
        take_line(first, &mut self.sampler, &mut kept);
        let mut ended = false;
        if let Some(lines) = self.conn.as_mut() {
            while seen < MAX_BATCH {
                match lines.next().now_or_never() {
                    Some(Some(value)) => {
                        seen += 1;
                        take_line(&value, &mut self.sampler, &mut kept);
                    }
                    Some(None) => {
                        ended = true;
                        break;
                    }
                    None => break,
                }
            }
        }
        let sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.store
            .lock()
            .push_batch(kept, seen as u64, self.sampler.every, sec);
        if ended {
            self.lost();
        }
    }
}

/// Whether the server really started MONITOR.
#[derive(Debug, PartialEq, Eq)]
enum Confirmed {
    Yes,
    Refused,
    Lost,
}

/// redis-rs reads the reply to `MONITOR` and drops it even when it is an
/// error, so a refused MONITOR looks like a quiet server. Send a unique
/// `PING` on another connection and wait to see it on the stream. Lines
/// before it are dropped. A `PING` we may not send proves nothing either way,
/// so it counts as started.
async fn confirm_started(client: &redis::Client, lines: &mut Lines) -> Confirmed {
    let nonce = format!(
        "rrover-monitor-check-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let opened = tokio::time::timeout(CONNECT_TIMEOUT, client.get_multiplexed_async_connection());
    let Ok(Ok(mut probe)) = opened.await else {
        return Confirmed::Lost;
    };
    let mut ping = redis::cmd("PING");
    ping.arg(&nonce);
    let sent = ping.query_async::<Value>(&mut probe);
    match tokio::time::timeout(CONNECT_TIMEOUT, sent).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) if is_connection_error(&err) => return Confirmed::Lost,
        Ok(Err(err)) => {
            log::debug!("MONITOR check not sent: {err}");
            return Confirmed::Yes;
        }
        Err(_) => return Confirmed::Lost,
    }
    let seen = async {
        while let Some(value) = lines.next().await {
            if line_text(&value).is_some_and(|text| text.contains(&nonce)) {
                return Confirmed::Yes;
            }
        }
        Confirmed::Lost
    };
    tokio::time::timeout(CONFIRM_TIMEOUT, seen)
        .await
        .unwrap_or(Confirmed::Refused)
}

/// The text of a MONITOR line.
fn line_text(value: &Value) -> Option<&str> {
    match value {
        Value::SimpleString(text) => Some(text),
        Value::BulkString(bytes) => std::str::from_utf8(bytes).ok(),
        _ => None,
    }
}

/// Parse `value` if the sampler keeps it. Skipped lines cost no parsing.
fn take_line(value: &Value, sampler: &mut Sampler, kept: &mut Vec<ParsedLine>) {
    if sampler.keep() {
        kept.extend(line_text(value).and_then(parse_line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(sampler: &mut Sampler, lines: usize) -> usize {
        (0..lines).filter(|_| sampler.keep()).count()
    }

    #[test]
    fn keeps_everything_under_the_limit() {
        let start = StdInstant::now();
        let mut s = Sampler::new(5000);
        s.roll(start);
        assert_eq!(kept(&mut s, 1000), 1000); // 4,000 / s
        s.roll(start + SAMPLE_WINDOW);
        assert_eq!(s.every, 1);
    }

    #[test]
    fn samples_above_the_limit_and_recovers() {
        let start = StdInstant::now();
        let mut s = Sampler::new(5000);
        s.roll(start);
        kept(&mut s, 12_500); // 50,000 / s
        s.roll(start + SAMPLE_WINDOW);
        assert_eq!(s.every, 10);
        assert_eq!(kept(&mut s, 1000), 100);

        s.roll(start + SAMPLE_WINDOW * 2);
        assert_eq!(s.every, 1, "1,000 lines in a window is 4,000 / s");
    }

    #[test]
    fn a_window_is_not_cut_short() {
        let start = StdInstant::now();
        let mut s = Sampler::new(10);
        s.roll(start);
        kept(&mut s, 1000);
        s.roll(start + SAMPLE_WINDOW / 2);
        assert_eq!(s.every, 1);
    }

    #[test]
    fn a_quiet_gap_does_not_read_as_a_burst() {
        let start = StdInstant::now();
        let mut s = Sampler::new(100);
        s.roll(start);
        kept(&mut s, 10);
        s.roll(start + Duration::from_mins(1));
        assert_eq!(s.every, 1);
    }

    #[test]
    fn a_zero_limit_cannot_divide_by_zero() {
        let start = StdInstant::now();
        let mut s = Sampler::new(0);
        s.roll(start);
        kept(&mut s, 10);
        s.roll(start + SAMPLE_WINDOW);
        assert_eq!(s.every, 40);
    }

    #[test]
    fn only_kept_lines_are_parsed_into_the_batch() {
        let mut s = Sampler::new(1);
        s.every = 2;
        let mut out = Vec::new();
        for _ in 0..4 {
            take_line(
                &Value::SimpleString(r#"1.0 [0 lua] "GET" "a""#.into()),
                &mut s,
                &mut out,
            );
        }
        assert_eq!(out.len(), 2);
        take_line(&Value::Nil, &mut s, &mut out);
        take_line(&Value::Nil, &mut s, &mut out);
        assert_eq!(out.len(), 2, "not a line");
    }
}
