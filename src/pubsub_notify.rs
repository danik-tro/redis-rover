//! Notification state machine of the Pub/Sub subscriptions. Pure: no I/O,
//! time comes in as `Instant`s, so every rule is unit-tested on synthetic
//! timelines.
//!
//! With notifications on, the first message of a subscription toasts, the
//! following ones only bump the unread badge, and after `cooldown` of quiet the
//! next message toasts again. A subscription that stays above `mute_rate` for
//! `mute_after` is muted until the user turns it back on.

use std::time::{Duration, Instant};

/// Notification state of one subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyState {
    /// No toasts (the default for new subscriptions).
    #[default]
    Off,
    On {
        /// When the last message was seen; `None` before the first one.
        last_msg_at: Option<Instant>,
        /// Since when the rate has been above `mute_rate`.
        above_since: Option<Instant>,
    },
    /// Muted for being too noisy; stays so until toggled.
    AutoMuted,
}

impl NotifyState {
    /// `N`: `Off` / `AutoMuted` → `On`, `On` → `Off`.
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::Off | Self::AutoMuted => Self::On {
                last_msg_at: None,
                above_since: None,
            },
            Self::On { .. } => Self::Off,
        }
    }
}

/// Something worth a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// First message after a quiet period.
    FirstMessage { target: String, preview: String },
    /// The subscription was muted for being too noisy.
    Muted { target: String },
}

/// Thresholds, from `pubsub.notify` in the config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NotifyPolicy {
    /// Quiet time before the next message toasts again.
    pub cooldown: Duration,
    /// msg/s above which a subscription counts as noisy.
    pub mute_rate: f64,
    /// How long it must stay noisy to be muted.
    pub mute_after: Duration,
}

/// Advance `state` by one tick: `new_msgs` arrived since the previous tick,
/// at `rate` msg/s. `first_preview` is the toast text for a first message.
pub fn on_tick(
    state: &mut NotifyState,
    target: &str,
    new_msgs: u64,
    rate: f64,
    first_preview: Option<&str>,
    now: Instant,
    policy: &NotifyPolicy,
) -> Option<Notice> {
    let NotifyState::On {
        last_msg_at,
        above_since,
    } = state
    else {
        return None;
    };

    let mut notice = None;
    if new_msgs > 0 {
        let quiet =
            last_msg_at.is_none_or(|at| now.saturating_duration_since(at) >= policy.cooldown);
        if quiet {
            notice = Some(Notice::FirstMessage {
                target: target.to_owned(),
                preview: first_preview.unwrap_or(target).to_owned(),
            });
        }
        *last_msg_at = Some(now);
    }

    if rate > policy.mute_rate {
        let since = *above_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= policy.mute_after {
            *state = NotifyState::AutoMuted;
            return Some(Notice::Muted {
                target: target.to_owned(),
            });
        }
    } else {
        *above_since = None;
    }
    notice
}

/// Toast texts for the notices of one tick: each `Muted` gets its own, first
/// messages share one (`3 subscriptions have new messages`).
pub fn toasts(notices: Vec<Notice>) -> Vec<String> {
    let mut out = Vec::new();
    let mut firsts = Vec::new();
    for notice in notices {
        match notice {
            Notice::Muted { target } => {
                out.push(format!("notifications muted for {target} (too noisy)"));
            }
            Notice::FirstMessage { preview, .. } => firsts.push(preview),
        }
    }
    match firsts.len() {
        0 => {}
        1 => out.insert(0, firsts.remove(0)),
        n => out.insert(0, format!("{n} subscriptions have new messages")),
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    const POLICY: NotifyPolicy = NotifyPolicy {
        cooldown: Duration::from_secs(30),
        mute_rate: 10.0,
        mute_after: Duration::from_secs(10),
    };

    /// A timeline: `t(s)` is `s` seconds after the start.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Self(Instant::now())
        }

        fn t(&self, secs: f64) -> Instant {
            self.0 + Duration::from_secs_f64(secs)
        }
    }

    fn on() -> NotifyState {
        NotifyState::Off.toggled()
    }

    fn tick(state: &mut NotifyState, new: u64, rate: f64, now: Instant) -> Option<Notice> {
        on_tick(
            state,
            "orders:*",
            new,
            rate,
            Some("orders:new: {}"),
            now,
            &POLICY,
        )
    }

    fn first() -> Notice {
        Notice::FirstMessage {
            target: "orders:*".into(),
            preview: "orders:new: {}".into(),
        }
    }

    fn muted() -> Notice {
        Notice::Muted {
            target: "orders:*".into(),
        }
    }

    #[test]
    fn off_never_notifies() {
        let c = Clock::new();
        let mut s = NotifyState::Off;
        assert_eq!(tick(&mut s, 5, 100.0, c.t(0.0)), None);
        assert_eq!(tick(&mut s, 5, 100.0, c.t(60.0)), None);
        assert_eq!(s, NotifyState::Off);
    }

    #[test]
    fn first_message_toasts() {
        let c = Clock::new();
        let mut s = on();
        assert_eq!(tick(&mut s, 0, 0.0, c.t(0.0)), None);
        assert_eq!(tick(&mut s, 1, 0.0, c.t(1.0)), Some(first()));
    }

    #[test]
    fn messages_inside_the_cooldown_dont_toast() {
        let c = Clock::new();
        let mut s = on();
        assert_eq!(tick(&mut s, 1, 1.0, c.t(0.0)), Some(first()));
        // A message every 20 s keeps it inside the 30 s cooldown.
        for secs in [5.0, 20.0, 40.0, 60.0, 80.0] {
            assert_eq!(tick(&mut s, 1, 1.0, c.t(secs)), None, "at {secs}");
        }
    }

    #[test]
    fn toasts_again_after_the_quiet_period() {
        let c = Clock::new();
        let mut s = on();
        assert_eq!(tick(&mut s, 1, 1.0, c.t(0.0)), Some(first()));
        // Quiet ticks don't count as messages.
        assert_eq!(tick(&mut s, 0, 0.0, c.t(15.0)), None);
        assert_eq!(tick(&mut s, 1, 1.0, c.t(29.9)), None);
        assert_eq!(tick(&mut s, 1, 1.0, c.t(60.0)), Some(first()));
    }

    #[test]
    fn auto_mutes_after_sustained_noise() {
        let c = Clock::new();
        let mut s = on();
        assert_eq!(tick(&mut s, 300, 300.0, c.t(0.0)), Some(first()));
        let mut t = 0.25;
        while t < 10.0 {
            assert_eq!(tick(&mut s, 75, 300.0, c.t(t)), None, "at {t}");
            t += 0.25;
        }
        assert_eq!(tick(&mut s, 75, 300.0, c.t(10.0)), Some(muted()));
        assert_eq!(s, NotifyState::AutoMuted);
        // Once muted, nothing more.
        assert_eq!(tick(&mut s, 75, 300.0, c.t(10.25)), None);
    }

    #[test]
    fn rate_at_the_threshold_is_not_noisy() {
        let c = Clock::new();
        let mut s = on();
        for secs in 0..30 {
            tick(&mut s, 10, 10.0, c.t(f64::from(secs)));
        }
        assert!(matches!(s, NotifyState::On { .. }));
    }

    #[test]
    fn short_burst_doesnt_mute() {
        let c = Clock::new();
        let mut s = on();
        assert_eq!(tick(&mut s, 50, 50.0, c.t(0.0)), Some(first()));
        assert_eq!(tick(&mut s, 50, 50.0, c.t(9.0)), None);
        // The rate drops before 10 s: the noisy streak starts over.
        assert_eq!(tick(&mut s, 1, 1.0, c.t(9.5)), None);
        assert_eq!(tick(&mut s, 50, 50.0, c.t(12.0)), None);
        assert_eq!(tick(&mut s, 50, 50.0, c.t(21.0)), None);
        assert!(matches!(s, NotifyState::On { .. }));
        assert_eq!(tick(&mut s, 50, 50.0, c.t(22.0)), Some(muted()));
    }

    #[test]
    fn stays_muted_when_the_rate_drops() {
        let c = Clock::new();
        let mut s = on();
        tick(&mut s, 100, 100.0, c.t(0.0));
        assert_eq!(tick(&mut s, 100, 100.0, c.t(10.0)), Some(muted()));
        assert_eq!(tick(&mut s, 0, 0.0, c.t(100.0)), None);
        assert_eq!(tick(&mut s, 1, 0.0, c.t(200.0)), None);
        assert_eq!(s, NotifyState::AutoMuted);
    }

    #[test]
    fn toggle_unmutes() {
        let c = Clock::new();
        let mut s = on();
        tick(&mut s, 100, 100.0, c.t(0.0));
        tick(&mut s, 100, 100.0, c.t(10.0));
        assert_eq!(s, NotifyState::AutoMuted);
        s = s.toggled();
        assert_eq!(
            s,
            NotifyState::On {
                last_msg_at: None,
                above_since: None
            }
        );
        // Back on: the next message toasts like the first one.
        assert_eq!(tick(&mut s, 1, 1.0, c.t(11.0)), Some(first()));
        assert_eq!(s.toggled(), NotifyState::Off);
    }

    #[test]
    fn missing_preview_falls_back_to_the_target() {
        let c = Clock::new();
        let mut s = on();
        assert_eq!(
            on_tick(&mut s, "chat", 1, 1.0, None, c.t(0.0), &POLICY),
            Some(Notice::FirstMessage {
                target: "chat".into(),
                preview: "chat".into()
            })
        );
    }

    fn first_of(target: &str) -> Notice {
        Notice::FirstMessage {
            target: target.into(),
            preview: format!("{target}: x"),
        }
    }

    #[test]
    fn several_subscriptions_in_one_tick_collapse() {
        assert_eq!(toasts(vec![]), Vec::<String>::new());
        assert_eq!(toasts(vec![first_of("a")]), vec!["a: x"]);
        assert_eq!(
            toasts(vec![first_of("a"), first_of("b"), first_of("c")]),
            vec!["3 subscriptions have new messages"]
        );
        // Muted always gets its own toast.
        assert_eq!(
            toasts(vec![
                first_of("a"),
                Notice::Muted {
                    target: "noisy".into()
                },
                first_of("b"),
            ]),
            vec![
                "2 subscriptions have new messages",
                "notifications muted for noisy (too noisy)"
            ]
        );
    }
}
