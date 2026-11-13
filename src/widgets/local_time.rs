//! Wall-clock times in the local time zone, for labels like `14:22:07`.

use std::time::{SystemTime, UNIX_EPOCH};

/// `t` as local `HH:MM:SS`.
#[must_use]
pub fn hms(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let (h, m, s) = local_hms(secs);
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(unix)]
fn local_hms(secs: u64) -> (u64, u64, u64) {
    let Ok(t) = libc::time_t::try_from(secs) else {
        return utc_hms(secs);
    };
    // SAFETY: `tm` is plain data; `localtime_r` only writes into it and is
    // thread-safe (unlike `localtime`).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&raw const t, &raw mut tm) }.is_null() {
        return utc_hms(secs);
    }
    let part = |v: libc::c_int| u64::try_from(v).unwrap_or(0);
    (part(tm.tm_hour), part(tm.tm_min), part(tm.tm_sec))
}

#[cfg(not(unix))]
fn local_hms(secs: u64) -> (u64, u64, u64) {
    utc_hms(secs)
}

fn utc_hms(secs: u64) -> (u64, u64, u64) {
    let day = secs % 86_400;
    (day / 3600, day % 3600 / 60, day % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_clock_parts() {
        assert_eq!(utc_hms(0), (0, 0, 0));
        assert_eq!(utc_hms(86_400 + 3_723), (1, 2, 3));
        assert_eq!(hms(UNIX_EPOCH).len(), "00:00:00".len());
    }
}
