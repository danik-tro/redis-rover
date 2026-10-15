#![allow(dead_code)] // some snapshot fields/accessors are kept for completeness but not rendered yet
//! Metrics engine for the live dashboard: bounded series, a numeric view of one
//! `INFO` reply, and the rate/reset logic that turns successive snapshots into
//! chartable points. Pure logic, no I/O.

use std::{collections::HashMap, collections::VecDeque, time::Instant};

/// Bounded ring buffer of samples; the oldest point is evicted at capacity.
#[derive(Clone, Debug)]
pub struct Series {
    cap: usize,
    buf: VecDeque<f64>,
}

impl Series {
    /// Create an empty series holding at most `cap` points.
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            buf: VecDeque::with_capacity(cap),
        }
    }

    /// Append a point, evicting the oldest one when full.
    pub fn push(&mut self, v: f64) {
        if self.cap == 0 {
            return;
        }
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }

    /// Most recent point.
    pub fn last(&self) -> Option<f64> {
        self.buf.back().copied()
    }

    /// Largest point currently held.
    pub fn max(&self) -> Option<f64> {
        self.buf.iter().copied().reduce(f64::max)
    }

    /// Number of points held.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the series holds no points.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Last `n` points, oldest first.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = f64> + '_ {
        self.buf
            .iter()
            .copied()
            .skip(self.buf.len().saturating_sub(n))
    }

    /// New series with the same capacity holding only the last `n` points.
    pub fn tail_series(&self, n: usize) -> Series {
        Series {
            cap: self.cap,
            buf: self.tail(n).collect(),
        }
    }

    /// Drop all points.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// Numeric view of one `INFO` reply. Every field is optional because old
/// servers and managed providers hide some of them.
#[derive(Clone, Debug)]
pub struct InfoSnapshot {
    pub taken_at: Instant,
    pub uptime_secs: Option<u64>,
    pub total_commands: Option<u64>,
    pub instantaneous_ops: Option<u64>,
    pub used_memory: Option<u64>,
    pub used_memory_rss: Option<u64>,
    pub used_memory_peak: Option<u64>,
    pub maxmemory: Option<u64>,
    pub total_system_memory: Option<u64>,
    pub mem_fragmentation_ratio: Option<f64>,
    pub cpu_sys: Option<f64>,
    pub cpu_user: Option<f64>,
    pub net_in_bytes: Option<u64>,
    pub net_out_bytes: Option<u64>,
    pub keyspace_hits: Option<u64>,
    pub keyspace_misses: Option<u64>,
    pub connected_clients: Option<u64>,
    pub blocked_clients: Option<u64>,
    pub maxclients: Option<u64>,
    pub rejected_connections: Option<u64>,
    pub evicted_keys: Option<u64>,
    pub expired_keys: Option<u64>,
    /// Summed from the `dbN` lines of the `# Keyspace` section.
    pub total_keys: Option<u64>,
    pub role: Option<String>,
    pub connected_slaves: Option<u64>,
    pub rdb_changes_since_last_save: Option<u64>,
    pub cluster_enabled: bool,
}

fn get_u64(map: &HashMap<&str, &str>, k: &str) -> Option<u64> {
    map.get(k)?.parse().ok()
}

fn get_f64(map: &HashMap<&str, &str>, k: &str) -> Option<f64> {
    map.get(k)?.parse().ok()
}

/// `"keys=12,expires=0,avg_ttl=0"` → `Some(12)`.
fn parse_keyspace_line(v: &str) -> Option<u64> {
    v.split(',')
        .find_map(|part| part.trim().strip_prefix("keys="))?
        .parse()
        .ok()
}

impl InfoSnapshot {
    /// A snapshot with every field unset.
    pub fn empty(taken_at: Instant) -> Self {
        Self {
            taken_at,
            uptime_secs: None,
            total_commands: None,
            instantaneous_ops: None,
            used_memory: None,
            used_memory_rss: None,
            used_memory_peak: None,
            maxmemory: None,
            total_system_memory: None,
            mem_fragmentation_ratio: None,
            cpu_sys: None,
            cpu_user: None,
            net_in_bytes: None,
            net_out_bytes: None,
            keyspace_hits: None,
            keyspace_misses: None,
            connected_clients: None,
            blocked_clients: None,
            maxclients: None,
            rejected_connections: None,
            evicted_keys: None,
            expired_keys: None,
            total_keys: None,
            role: None,
            connected_slaves: None,
            rdb_changes_since_last_save: None,
            cluster_enabled: false,
        }
    }

    /// Build a snapshot from a parsed `INFO` map. The time is a parameter so
    /// tests control it.
    pub fn from_map(map: &HashMap<&str, &str>, taken_at: Instant) -> Self {
        let total_keys = Some(
            map.iter()
                .filter(|(k, _)| {
                    k.strip_prefix("db")
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
                .filter_map(|(_, v)| parse_keyspace_line(v))
                .sum(),
        );

        Self {
            taken_at,
            uptime_secs: get_u64(map, "uptime_in_seconds"),
            total_commands: get_u64(map, "total_commands_processed"),
            instantaneous_ops: get_u64(map, "instantaneous_ops_per_sec"),
            used_memory: get_u64(map, "used_memory"),
            used_memory_rss: get_u64(map, "used_memory_rss"),
            used_memory_peak: get_u64(map, "used_memory_peak"),
            maxmemory: get_u64(map, "maxmemory"),
            total_system_memory: get_u64(map, "total_system_memory"),
            mem_fragmentation_ratio: get_f64(map, "mem_fragmentation_ratio"),
            cpu_sys: get_f64(map, "used_cpu_sys"),
            cpu_user: get_f64(map, "used_cpu_user"),
            net_in_bytes: get_u64(map, "total_net_input_bytes"),
            net_out_bytes: get_u64(map, "total_net_output_bytes"),
            keyspace_hits: get_u64(map, "keyspace_hits"),
            keyspace_misses: get_u64(map, "keyspace_misses"),
            connected_clients: get_u64(map, "connected_clients"),
            blocked_clients: get_u64(map, "blocked_clients"),
            maxclients: get_u64(map, "maxclients"),
            rejected_connections: get_u64(map, "rejected_connections"),
            evicted_keys: get_u64(map, "evicted_keys"),
            expired_keys: get_u64(map, "expired_keys"),
            total_keys,
            role: map.get("role").map(|s| (*s).to_string()),
            connected_slaves: get_u64(map, "connected_slaves"),
            rdb_changes_since_last_save: get_u64(map, "rdb_changes_since_last_save"),
            cluster_enabled: map.get("cluster_enabled") == Some(&"1"),
        }
    }
}

/// Outcome of comparing a counter against its baseline.
enum Rate {
    Value(f64),
    Reset,
    Missing,
}

#[allow(clippy::cast_precision_loss)]
fn rate(prev: Option<u64>, cur: Option<u64>, dt: f64) -> Rate {
    match (prev, cur) {
        (Some(p), Some(c)) => match c.checked_sub(p) {
            Some(d) => Rate::Value(d as f64 / dt),
            None => Rate::Reset,
        },
        _ => Rate::Missing,
    }
}

fn rate_f64(prev: Option<f64>, cur: Option<f64>, dt: f64) -> Rate {
    match (prev, cur) {
        (Some(p), Some(c)) if c < p => Rate::Reset,
        (Some(p), Some(c)) => Rate::Value((c - p) / dt),
        _ => Rate::Missing,
    }
}

impl Rate {
    fn value(&self) -> Option<f64> {
        match self {
            Rate::Value(v) => Some(*v),
            _ => None,
        }
    }

    fn is_reset(&self) -> bool {
        matches!(self, Rate::Reset)
    }
}

/// Latest derived values, for titles and stat tiles.
#[derive(Clone, Debug, Default)]
pub struct MetricsSample {
    pub ops_per_sec: Option<f64>,
    pub cpu_pct: Option<f64>,
    pub net_in_bps: Option<f64>,
    pub net_out_bps: Option<f64>,
    /// Windowed hit ratio, `0.0..=1.0`.
    pub hit_rate: Option<f64>,
    pub hit_rate_lifetime: Option<f64>,
    pub evicted_ps: Option<f64>,
    pub expired_ps: Option<f64>,
    pub rejected_ps: Option<f64>,
}

/// Summary of `CLUSTER INFO`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClusterSummary {
    pub state_ok: bool,
    pub known_nodes: u64,
    pub size: u64,
    pub slots_ok: u64,
    pub slots_pfail: u64,
    pub slots_fail: u64,
}

/// Parse the text of a `CLUSTER INFO` reply. Missing numbers become 0.
pub fn parse_cluster_info(raw: &str) -> ClusterSummary {
    let map: HashMap<&str, &str> = raw
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let n = |k: &str| get_u64(&map, k).unwrap_or(0);

    ClusterSummary {
        state_ok: map.get("cluster_state") == Some(&"ok"),
        known_nodes: n("cluster_known_nodes"),
        size: n("cluster_size"),
        slots_ok: n("cluster_slots_ok"),
        slots_pfail: n("cluster_slots_pfail"),
        slots_fail: n("cluster_slots_fail"),
    }
}

/// Rolling history of dashboard metrics, fed one [`InfoSnapshot`] at a time.
#[derive(Clone, Debug)]
pub struct MetricsHistory {
    capacity: usize,
    pub ops: Series,
    pub cpu_pct: Series,
    pub net_in: Series,
    pub net_out: Series,
    pub hit_rate: Series,
    pub used_memory: Series,
    pub clients: Series,
    pub evicted_ps: Series,
    pub expired_ps: Series,
    pub total_keys: Series,
    baseline: Option<InfoSnapshot>,
    pub latest_snapshot: Option<InfoSnapshot>,
    pub latest: Option<MetricsSample>,
    pub cluster: Option<ClusterSummary>,
    pub samples_recorded: u64,
}

#[allow(clippy::cast_precision_loss)]
fn push_opt(series: &mut Series, v: Option<u64>) {
    if let Some(v) = v {
        series.push(v as f64);
    }
}

fn ratio(hits: Option<u64>, misses: Option<u64>) -> Option<f64> {
    let (h, m) = (hits?, misses?);
    let total = h.checked_add(m)?;
    #[allow(clippy::cast_precision_loss)]
    (total > 0).then(|| h as f64 / total as f64)
}

impl MetricsHistory {
    /// Create a history whose series each hold `capacity` points.
    pub fn new(capacity: usize) -> Self {
        let s = || Series::new(capacity);
        Self {
            capacity,
            ops: s(),
            cpu_pct: s(),
            net_in: s(),
            net_out: s(),
            hit_rate: s(),
            used_memory: s(),
            clients: s(),
            evicted_ps: s(),
            expired_ps: s(),
            total_keys: s(),
            baseline: None,
            latest_snapshot: None,
            latest: None,
            cluster: None,
            samples_recorded: 0,
        }
    }

    /// Fold one snapshot into the history: push gauges, then derive rates
    /// against the previous snapshot using the measured time delta. A counter
    /// that went backwards (server restart, `CONFIG RESETSTAT`) re-baselines
    /// and skips the rate points for this tick.
    pub fn record(&mut self, snap: InfoSnapshot) {
        push_opt(&mut self.used_memory, snap.used_memory);
        push_opt(&mut self.clients, snap.connected_clients);
        push_opt(&mut self.total_keys, snap.total_keys);

        let Some(base) = self.baseline.as_ref() else {
            push_opt(&mut self.ops, snap.instantaneous_ops);
            #[allow(clippy::cast_precision_loss)]
            let ops = snap.instantaneous_ops.map(|v| v as f64);
            self.latest = Some(MetricsSample {
                ops_per_sec: ops,
                ..MetricsSample::default()
            });
            self.baseline = Some(snap.clone());
            self.latest_snapshot = Some(snap);
            return;
        };

        // Reset detection (dt is irrelevant here, so use 1.0).
        let uptime_reset = matches!(
            (base.uptime_secs, snap.uptime_secs),
            (Some(p), Some(c)) if c < p
        );
        let counters = [
            (base.total_commands, snap.total_commands),
            (base.net_in_bytes, snap.net_in_bytes),
            (base.net_out_bytes, snap.net_out_bytes),
            (base.keyspace_hits, snap.keyspace_hits),
            (base.keyspace_misses, snap.keyspace_misses),
            (base.evicted_keys, snap.evicted_keys),
            (base.expired_keys, snap.expired_keys),
            (base.rejected_connections, snap.rejected_connections),
        ];
        let counter_reset = counters.iter().any(|(p, c)| rate(*p, *c, 1.0).is_reset())
            || rate_f64(base.cpu_sys, snap.cpu_sys, 1.0).is_reset()
            || rate_f64(base.cpu_user, snap.cpu_user, 1.0).is_reset();
        if uptime_reset || counter_reset {
            // No valid delta this tick: drop the stale rates, keep the
            // point-in-time lifetime ratio.
            self.latest = Some(MetricsSample {
                hit_rate_lifetime: ratio(snap.keyspace_hits, snap.keyspace_misses),
                ..MetricsSample::default()
            });
            self.baseline = Some(snap.clone());
            self.latest_snapshot = Some(snap);
            return;
        }

        let dt = snap.taken_at.duration_since(base.taken_at).as_secs_f64();
        if dt <= 0.0 {
            return;
        }

        let r = |p, c| rate(p, c, dt).value();
        let ops = r(base.total_commands, snap.total_commands);
        let net_in = r(base.net_in_bytes, snap.net_in_bytes);
        let net_out = r(base.net_out_bytes, snap.net_out_bytes);
        let evicted = r(base.evicted_keys, snap.evicted_keys);
        let expired = r(base.expired_keys, snap.expired_keys);
        let rejected = r(base.rejected_connections, snap.rejected_connections);

        let cpu_pct = match (
            rate_f64(base.cpu_sys, snap.cpu_sys, dt).value(),
            rate_f64(base.cpu_user, snap.cpu_user, dt).value(),
        ) {
            (Some(s), Some(u)) => Some((s + u) * 100.0),
            _ => None,
        };

        // Δ counts (dt = 1.0 makes the "rate" a plain delta).
        let hit_rate = match (
            rate(base.keyspace_hits, snap.keyspace_hits, 1.0).value(),
            rate(base.keyspace_misses, snap.keyspace_misses, 1.0).value(),
        ) {
            (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m)),
            _ => None,
        };

        for (series, v) in [
            (&mut self.ops, ops),
            (&mut self.cpu_pct, cpu_pct),
            (&mut self.net_in, net_in),
            (&mut self.net_out, net_out),
            (&mut self.hit_rate, hit_rate),
            (&mut self.evicted_ps, evicted),
            (&mut self.expired_ps, expired),
        ] {
            if let Some(v) = v {
                series.push(v);
            }
        }

        self.latest = Some(MetricsSample {
            ops_per_sec: ops,
            cpu_pct,
            net_in_bps: net_in,
            net_out_bps: net_out,
            hit_rate,
            hit_rate_lifetime: ratio(snap.keyspace_hits, snap.keyspace_misses),
            evicted_ps: evicted,
            expired_ps: expired,
            rejected_ps: rejected,
        });
        self.baseline = Some(snap.clone());
        self.latest_snapshot = Some(snap);
        self.samples_recorded += 1;
    }

    /// Forget the latest derived values (e.g. `INFO` failed) but keep the
    /// baseline so rates resume over the longer interval.
    pub fn mark_unavailable(&mut self) {
        self.latest = None;
    }

    /// Longest series length, i.e. how many points the widest panel can draw.
    pub fn window_len(&self) -> usize {
        self.series().iter().map(|s| s.len()).max().unwrap_or(0)
    }

    fn series(&self) -> [&Series; 10] {
        [
            &self.ops,
            &self.cpu_pct,
            &self.net_in,
            &self.net_out,
            &self.hit_rate,
            &self.used_memory,
            &self.clients,
            &self.evicted_ps,
            &self.expired_ps,
            &self.total_keys,
        ]
    }

    /// Copy of this history keeping only the last `n` points of each series.
    pub fn window(&self, n: usize) -> MetricsHistory {
        let t = |s: &Series| s.tail_series(n);
        MetricsHistory {
            capacity: self.capacity,
            ops: t(&self.ops),
            cpu_pct: t(&self.cpu_pct),
            net_in: t(&self.net_in),
            net_out: t(&self.net_out),
            hit_rate: t(&self.hit_rate),
            used_memory: t(&self.used_memory),
            clients: t(&self.clients),
            evicted_ps: t(&self.evicted_ps),
            expired_ps: t(&self.expired_ps),
            total_keys: t(&self.total_keys),
            baseline: self.baseline.clone(),
            latest_snapshot: self.latest_snapshot.clone(),
            latest: self.latest.clone(),
            cluster: self.cluster.clone(),
            samples_recorded: self.samples_recorded,
        }
    }

    /// Store the latest `CLUSTER INFO` summary.
    pub fn set_cluster(&mut self, c: Option<ClusterSummary>) {
        self.cluster = c;
    }

    /// Clear all series and the baseline. The cluster summary is kept.
    pub fn reset(&mut self) {
        for s in [
            &mut self.ops,
            &mut self.cpu_pct,
            &mut self.net_in,
            &mut self.net_out,
            &mut self.hit_rate,
            &mut self.used_memory,
            &mut self.clients,
            &mut self.evicted_ps,
            &mut self.expired_ps,
            &mut self.total_keys,
        ] {
            s.clear();
        }
        self.baseline = None;
        self.latest = None;
        self.samples_recorded = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;

    use super::*;
    use crate::redis_client::client::parse_info;

    fn snap(t0: Instant, secs: u64, f: impl FnOnce(&mut InfoSnapshot)) -> InfoSnapshot {
        let mut s = InfoSnapshot::empty(t0 + Duration::from_secs(secs));
        f(&mut s);
        s
    }

    #[test]
    fn series_evicts_oldest_at_capacity() {
        let mut s = Series::new(10);
        for i in 0..15 {
            s.push(f64::from(i));
        }
        assert_eq!(s.len(), 10);
        assert_eq!(s.tail(10).next(), Some(5.0));
        assert_eq!(s.last(), Some(14.0));
        assert_eq!(s.max(), Some(14.0));
    }

    #[test]
    fn series_tail_returns_last_n_in_order() {
        let mut s = Series::new(10);
        for i in 0..5 {
            s.push(f64::from(i));
        }
        assert_eq!(s.tail(3).collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
        assert_eq!(s.tail(99).count(), 5);
    }

    #[test]
    fn ops_rate_uses_measured_dt() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| s.total_commands = Some(1000)));
        h.record(snap(t0, 2, |s| s.total_commands = Some(3000)));
        assert_eq!(h.ops.last(), Some(1000.0));
        assert_eq!(h.latest.unwrap().ops_per_sec, Some(1000.0));
        assert_eq!(h.samples_recorded, 1);
    }

    #[test]
    fn first_sample_falls_back_to_instantaneous_ops() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| s.instantaneous_ops = Some(42)));
        assert_eq!(h.ops.last(), Some(42.0));
        assert_eq!(h.latest.unwrap().ops_per_sec, Some(42.0));
    }

    #[test]
    fn cpu_pct_sums_sys_and_user() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| {
            s.cpu_sys = Some(1.0);
            s.cpu_user = Some(2.0);
        }));
        h.record(snap(t0, 1, |s| {
            s.cpu_sys = Some(1.1);
            s.cpu_user = Some(2.15);
        }));
        let v = h.cpu_pct.last().unwrap();
        assert!((v - 25.0).abs() < 1e-9, "{v}");
    }

    #[test]
    fn net_rates_in_and_out() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| {
            s.net_in_bytes = Some(0);
            s.net_out_bytes = Some(0);
        }));
        h.record(snap(t0, 2, |s| {
            s.net_in_bytes = Some(2000);
            s.net_out_bytes = Some(6000);
        }));
        assert_eq!(h.net_in.last(), Some(1000.0));
        assert_eq!(h.net_out.last(), Some(3000.0));
    }

    #[test]
    fn hit_rate_none_without_lookups() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        let f = |s: &mut InfoSnapshot| {
            s.keyspace_hits = Some(5);
            s.keyspace_misses = Some(5);
        };
        h.record(snap(t0, 0, f));
        h.record(snap(t0, 1, f));
        assert!(h.hit_rate.is_empty());
        assert_eq!(h.latest.unwrap().hit_rate, None);
    }

    #[test]
    fn hit_rate_windowed() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| {
            s.keyspace_hits = Some(100);
            s.keyspace_misses = Some(100);
        }));
        h.record(snap(t0, 1, |s| {
            s.keyspace_hits = Some(190);
            s.keyspace_misses = Some(110);
        }));
        assert_eq!(h.hit_rate.last(), Some(0.9));
        let l = h.latest.unwrap();
        assert_eq!(l.hit_rate, Some(0.9));
        assert!((l.hit_rate_lifetime.unwrap() - 190.0 / 300.0).abs() < 1e-9);
    }

    #[test]
    fn counter_decrease_resets_baseline_and_skips_rates() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| {
            s.total_commands = Some(5000);
            s.rejected_connections = Some(7);
            s.keyspace_hits = Some(900);
            s.keyspace_misses = Some(100);
        }));
        h.record(snap(t0, 1, |s| {
            s.total_commands = Some(10);
            s.rejected_connections = Some(0);
            s.keyspace_hits = Some(3);
            s.keyspace_misses = Some(1);
        }));
        assert!(h.ops.is_empty());
        assert_eq!(h.samples_recorded, 0);
        let l = h.latest.as_ref().unwrap();
        assert_eq!(l.ops_per_sec, None);
        assert_eq!(l.rejected_ps, None);
        assert_eq!(l.hit_rate, None);
        assert_eq!(l.hit_rate_lifetime, Some(0.75));
        // New baseline is the post-reset snapshot.
        h.record(snap(t0, 2, |s| s.total_commands = Some(110)));
        assert_eq!(h.ops.last(), Some(100.0));
    }

    #[test]
    fn uptime_decrease_resets_baseline() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| {
            s.uptime_secs = Some(1000);
            s.total_commands = Some(10);
        }));
        h.record(snap(t0, 1, |s| {
            s.uptime_secs = Some(2);
            s.total_commands = Some(20);
        }));
        assert!(h.ops.is_empty());
        assert_eq!(h.latest.as_ref().unwrap().ops_per_sec, None);
        h.record(snap(t0, 2, |s| {
            s.uptime_secs = Some(3);
            s.total_commands = Some(50);
        }));
        assert_eq!(h.ops.last(), Some(30.0));
    }

    #[test]
    fn window_keeps_last_n_points() {
        let mut h = MetricsHistory::new(10);
        for v in 0..6 {
            h.used_memory.push(f64::from(v));
        }
        let w = h.window(2);
        assert_eq!(w.used_memory.tail(10).collect::<Vec<_>>(), vec![4.0, 5.0]);
        assert_eq!(h.used_memory.len(), 6);
    }

    #[test]
    fn window_len_counts_any_series() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        assert_eq!(h.window_len(), 0);
        h.record(snap(t0, 0, |s| s.used_memory = Some(1)));
        assert_eq!(h.window_len(), 1);
    }

    #[test]
    fn mark_unavailable_keeps_baseline() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| s.total_commands = Some(0)));
        h.mark_unavailable();
        assert!(h.latest.is_none());
        h.record(snap(t0, 4, |s| s.total_commands = Some(400)));
        assert_eq!(h.latest.unwrap().ops_per_sec, Some(100.0));
    }

    #[test]
    fn missing_fields_leave_series_empty() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |_| {}));
        h.record(snap(t0, 1, |_| {}));
        assert!(h.ops.is_empty());
        assert!(h.cpu_pct.is_empty());
        assert!(h.net_in.is_empty());
        assert!(h.used_memory.is_empty());
        assert!(h.clients.is_empty());
        assert!(h.total_keys.is_empty());
    }

    #[test]
    fn gauges_pushed_on_first_sample() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.record(snap(t0, 0, |s| {
            s.used_memory = Some(2048);
            s.connected_clients = Some(3);
            s.total_keys = Some(7);
        }));
        assert_eq!(h.used_memory.last(), Some(2048.0));
        assert_eq!(h.clients.last(), Some(3.0));
        assert_eq!(h.total_keys.last(), Some(7.0));
    }

    #[test]
    fn parse_keyspace_lines_sums_all_dbs() {
        let raw = "# Keyspace\r\ndb0:keys=10,expires=0,avg_ttl=0\r\ndb3:keys=5,expires=1,avg_ttl=100\r\ndbsize:keys=99\r\n";
        let map = parse_info(raw);
        let s = InfoSnapshot::from_map(&map, Instant::now());
        assert_eq!(s.total_keys, Some(15));
        assert_eq!(parse_keyspace_line("keys=12,expires=0,avg_ttl=0"), Some(12));
        assert_eq!(parse_keyspace_line("expires=0"), None);
    }

    #[test]
    fn parse_cluster_info_reads_counts_and_state() {
        let raw = "cluster_state:ok\r\ncluster_slots_assigned:16384\r\ncluster_slots_ok:16384\r\ncluster_slots_pfail:1\r\ncluster_slots_fail:2\r\ncluster_known_nodes:6\r\ncluster_size:3\r\n";
        let c = parse_cluster_info(raw);
        assert_eq!(
            c,
            ClusterSummary {
                state_ok: true,
                known_nodes: 6,
                size: 3,
                slots_ok: 16384,
                slots_pfail: 1,
                slots_fail: 2,
            }
        );
        assert!(!parse_cluster_info("cluster_state:fail\r\n").state_ok);
        assert_eq!(parse_cluster_info(""), ClusterSummary::default());
    }

    #[test]
    fn reset_clears_series_and_baseline() {
        let t0 = Instant::now();
        let mut h = MetricsHistory::new(10);
        h.set_cluster(Some(ClusterSummary::default()));
        h.record(snap(t0, 0, |s| {
            s.total_commands = Some(0);
            s.used_memory = Some(1);
        }));
        h.record(snap(t0, 1, |s| {
            s.total_commands = Some(10);
            s.used_memory = Some(2);
        }));
        h.reset();
        assert!(h.ops.is_empty());
        assert!(h.used_memory.is_empty());
        assert!(h.latest.is_none());
        assert_eq!(h.samples_recorded, 0);
        assert!(h.cluster.is_some());
        // Baseline gone: next record is a first sample again.
        h.record(snap(t0, 2, |s| s.total_commands = Some(100)));
        assert!(h.ops.is_empty());
    }

    #[test]
    fn real_info_text_parses_into_snapshot() {
        let raw = "# Server\r\nuptime_in_seconds:120\r\n\r\n# Clients\r\nconnected_clients:4\r\n\
                   # Memory\r\nused_memory:1048576\r\nmem_fragmentation_ratio:1.52\r\n\
                   # Stats\r\ntotal_commands_processed:777\r\nkeyspace_hits:3\r\n\
                   # Replication\r\nrole:master\r\n# Cluster\r\ncluster_enabled:0\r\n\
                   # Keyspace\r\ndb0:keys=12,expires=0,avg_ttl=0\r\n";
        let map = parse_info(raw);
        let s = InfoSnapshot::from_map(&map, Instant::now());
        assert_eq!(s.uptime_secs, Some(120));
        assert_eq!(s.connected_clients, Some(4));
        assert_eq!(s.used_memory, Some(1_048_576));
        assert_eq!(s.mem_fragmentation_ratio, Some(1.52));
        assert_eq!(s.total_commands, Some(777));
        assert_eq!(s.role.as_deref(), Some("master"));
        assert_eq!(s.total_keys, Some(12));
        assert!(!s.cluster_enabled);
        assert_eq!(s.keyspace_misses, None);
    }
}
