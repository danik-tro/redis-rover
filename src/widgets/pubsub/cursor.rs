use crate::{
    redis_client::pubsub_store::{MessageBuffer, Payload, PubSubMessage},
    widgets::text_view::{TextCache, TextView},
};

/// A paused log: what the user looks at, by sequence number so evictions
/// don't shift it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Paused {
    /// Selected entry.
    selected: u64,
    /// First entry shown.
    top: u64,
    /// Newest entry when the log was paused; what came after is `+N new`.
    newest_at_pause: u64,
    /// The paused rows were evicted and the view jumped to the oldest one.
    evicted: bool,
}

/// Position in a subscription's log: following the newest entry (default)
/// or paused on one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogCursor {
    paused: Option<Paused>,
    /// Rows the log showed on the last render.
    height: usize,
}

impl LogCursor {
    pub fn is_paused(&self) -> bool {
        self.paused.is_some()
    }

    pub fn follow(&mut self) {
        self.paused = None;
    }

    /// Freeze the view as it is (the newest `height` rows), selecting the
    /// newest entry. Nothing to pause on an empty buffer.
    fn pause(&mut self, buf: &MessageBuffer) -> bool {
        let Some(last) = buf.last_seq() else {
            return false;
        };
        let top = buf
            .get(buf.len().saturating_sub(self.height.max(1)))
            .map_or(last, |m| m.seq);
        self.paused = Some(Paused {
            selected: last,
            top,
            newest_at_pause: last,
            evicted: false,
        });
        true
    }

    /// Space: pause, or resume following.
    pub fn toggle_pause(&mut self, buf: &MessageBuffer) {
        if self.is_paused() {
            self.follow();
        } else {
            self.pause(buf);
        }
    }

    /// Index of the selected entry (the oldest kept one if it was evicted).
    fn selected_index(paused: &Paused, buf: &MessageBuffer) -> usize {
        buf.index_of(paused.selected)
            .unwrap_or_else(|i| i)
            .min(buf.len().saturating_sub(1))
    }

    /// `k`: select the previous entry. Pauses a following log first.
    pub fn up(&mut self, buf: &MessageBuffer) {
        if !self.is_paused() && !self.pause(buf) {
            return;
        }
        let Some(paused) = self.paused.as_mut() else {
            return;
        };
        let i = Self::selected_index(paused, buf);
        if let Some(prev) = i.checked_sub(1).and_then(|i| buf.get(i)) {
            paused.selected = prev.seq;
            paused.evicted = false;
        }
    }

    /// `j`: select the next entry; past the newest one, follow again.
    pub fn down(&mut self, buf: &MessageBuffer) {
        let Some(paused) = self.paused.as_mut() else {
            return;
        };
        let i = Self::selected_index(paused, buf);
        match buf.get(i + 1) {
            Some(next) => {
                paused.selected = next.seq;
                paused.evicted = false;
            }
            None => self.follow(),
        }
    }

    /// Fit the view to `buf` and `height` rows: returns the index of the
    /// first row shown and of the selected one (`None` when empty). A
    /// paused view whose rows were evicted moves to the oldest entry still
    /// kept; an empty buffer (cleared) follows again.
    pub fn sync(&mut self, buf: &MessageBuffer, height: usize) -> (usize, Option<usize>) {
        self.height = height;
        let height = height.max(1);
        let (Some(first), len) = (buf.first_seq(), buf.len()) else {
            self.follow();
            return (0, None);
        };
        let Some(paused) = self.paused.as_mut() else {
            return (len.saturating_sub(height), Some(len - 1));
        };
        if paused.selected < first {
            paused.selected = first;
            paused.evicted = true;
        }
        let selected = Self::selected_index(paused, buf);
        let mut top = buf.index_of(paused.top.max(first)).unwrap_or_else(|i| i);
        if selected < top {
            top = selected;
        } else if selected >= top + height {
            top = selected + 1 - height;
        }
        paused.top = buf.get(top).map_or(first, |m| m.seq);
        (top, Some(selected))
    }

    /// Sequence number of the selected entry: the newest one while following.
    pub fn selected_seq(&self, buf: &MessageBuffer) -> Option<u64> {
        match self.paused {
            Some(paused) => buf
                .first_seq()
                .map(|first| paused.selected.max(first))
                .filter(|_| !buf.is_empty()),
            None => buf.last_seq(),
        }
    }

    /// Entries that arrived since the log was paused, counted by `seq`.
    pub fn new_count(&self, buf: &MessageBuffer) -> u64 {
        match (self.paused, buf.last_seq()) {
            (Some(paused), Some(last)) => last.saturating_sub(paused.newest_at_pause),
            _ => 0,
        }
    }

    pub fn evicted(&self) -> bool {
        self.paused.is_some_and(|p| p.evicted)
    }
}

/// The message detail pane: one message, its full payload in the text viewer.
pub(super) struct Detail {
    pub(super) msg: PubSubMessage,
    /// The pattern of a pattern subscription.
    pub(super) pattern: Option<String>,
    pub(super) text: TextCache,
    pub(super) view: TextView,
}

impl Detail {
    pub(super) fn new(msg: PubSubMessage, pattern: Option<String>) -> Self {
        let text = match &msg.payload {
            Payload::Text(text) => TextCache::from_text(text, !msg.truncated),
            binary @ Payload::Binary(_) => TextCache::from_lines(binary.hex_dump()),
        };
        Self {
            msg,
            pattern,
            text,
            view: TextView::default(),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::widgets::pubsub::view::log_state;
    use pretty_assertions::assert_eq;

    use crate::redis_client::pubsub_store::MessageBuffer;
    use crate::widgets::pubsub::tests::{fill, limits, shown};

    #[test]
    fn paused_view_keeps_its_rows_while_messages_arrive() {
        let mut buf = MessageBuffer::new(limits(1000));
        fill(&mut buf, 0, 10);
        let mut cursor = LogCursor::default();
        assert_eq!(shown(&mut cursor, &buf, 4), (vec![7, 8, 9, 10], Some(10)));

        cursor.toggle_pause(&buf);
        assert!(cursor.is_paused());
        fill(&mut buf, 10, 138);
        assert_eq!(shown(&mut cursor, &buf, 4), (vec![7, 8, 9, 10], Some(10)));
        assert_eq!(cursor.new_count(&buf), 128);
        assert_eq!(log_state(&cursor, &buf), "paused · +128 new");

        // Up moves the selection, the window follows it.
        for _ in 0..5 {
            cursor.up(&buf);
        }
        assert_eq!(shown(&mut cursor, &buf, 4), (vec![5, 6, 7, 8], Some(5)));

        cursor.toggle_pause(&buf);
        assert_eq!(log_state(&cursor, &buf), "following");
        assert_eq!(shown(&mut cursor, &buf, 2), (vec![137, 138], Some(138)));
    }

    #[test]
    fn up_pauses_and_down_past_the_end_follows() {
        let mut buf = MessageBuffer::new(limits(100));
        fill(&mut buf, 0, 5);
        let mut cursor = LogCursor::default();
        cursor.down(&buf);
        assert!(!cursor.is_paused(), "down while following does nothing");
        cursor.up(&buf);
        assert!(cursor.is_paused());
        assert_eq!(cursor.selected_seq(&buf), Some(4));
        cursor.down(&buf);
        assert_eq!(cursor.selected_seq(&buf), Some(5));
        assert!(cursor.is_paused());
        fill(&mut buf, 5, 6);
        cursor.down(&buf);
        assert_eq!(cursor.selected_seq(&buf), Some(6));
        cursor.down(&buf);
        assert!(!cursor.is_paused(), "past the newest row: following");

        // Nothing to pause on.
        let empty = MessageBuffer::new(limits(10));
        let mut cursor = LogCursor::default();
        cursor.toggle_pause(&empty);
        cursor.up(&empty);
        assert!(!cursor.is_paused());
    }

    #[test]
    fn eviction_moves_a_paused_view_to_the_oldest_row() {
        let mut buf = MessageBuffer::new(limits(5));
        fill(&mut buf, 0, 5);
        let mut cursor = LogCursor::default();
        cursor.sync(&buf, 3);
        cursor.up(&buf);
        assert_eq!(cursor.selected_seq(&buf), Some(4));
        fill(&mut buf, 5, 12);
        // Seqs 1..=7 are gone: the oldest kept is 8.
        assert_eq!(shown(&mut cursor, &buf, 3), (vec![8, 9, 10], Some(8)));
        assert!(cursor.evicted());
        assert_eq!(
            log_state(&cursor, &buf),
            "paused · +7 new · older messages evicted"
        );
        cursor.down(&buf);
        assert!(!cursor.evicted(), "moving clears the note");

        // Cleared (`c`): following again.
        buf.clear();
        assert_eq!(shown(&mut cursor, &buf, 3), (vec![], None));
        assert!(!cursor.is_paused());
    }
}
