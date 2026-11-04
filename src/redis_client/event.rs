use super::types::{ItemDelete, ItemEdit, KeyItem, NewKeySpec, RedisType};

#[derive(Clone, Debug)]
pub enum RedisEvent {
    FetchKeys,
    /// Switch the browsing connection to logical database `n`.
    SelectDb(i64),
    /// Fetch the page of the selected value described by
    /// `SharedState::value_state`.
    FetchValue,
    /// Count the keys matching `pattern` for the filter popup's live preview.
    /// Runs in its own task; a newer request cancels the older one.
    CountMatches {
        pattern: String,
    },
    /// Fetch a key's whole value for copying (`y`). Runs in its own task and
    /// leaves the text in `SharedState::yank`.
    FetchFullValue {
        key: String,
        r_type: RedisType,
    },
    SetString {
        key: String,
        value: String,
    },
    DeleteKey {
        key: String,
    },
    /// Set a key-level TTL. `secs <= 0` clears the TTL (`PERSIST`).
    SetTtl {
        key: String,
        secs: i64,
    },
    /// Create a new key from the add-key wizard. Rejected if the key exists.
    CreateKey {
        key: String,
        spec: NewKeySpec,
    },
    /// Append a single element to an existing collection (in-collection `i`).
    AddItem {
        key: String,
        item: KeyItem,
    },
    /// Edit a single collection element in place (in-collection `e`).
    EditItem {
        key: String,
        edit: ItemEdit,
    },
    /// Delete a single collection element (in-collection `d`).
    DeleteItem {
        key: String,
        delete: ItemDelete,
    },
    /// List the server's active channels (`PUBSUB CHANNELS` / `NUMSUB` /
    /// `NUMPAT`) into `SharedState::pubsub`.
    FetchPubSubChannels,
    /// `PUBLISH channel payload`. The payload travels in the event, not in an
    /// `Action`, so it isn't debug-logged as one.
    Publish {
        channel: String,
        payload: String,
    },
}

impl RedisEvent {
    /// True for events that change data. `--read-only` blocks these.
    ///
    /// No `_` arm on purpose: a new variant must pick a side.
    pub fn is_write(&self) -> bool {
        match self {
            Self::FetchKeys
            | Self::FetchValue
            | Self::SelectDb(_)
            | Self::CountMatches { .. }
            | Self::FetchFullValue { .. }
            | Self::FetchPubSubChannels => false,
            Self::SetString { .. }
            | Self::DeleteKey { .. }
            | Self::SetTtl { .. }
            | Self::CreateKey { .. }
            | Self::AddItem { .. }
            | Self::EditItem { .. }
            | Self::DeleteItem { .. }
            | Self::Publish { .. } => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_are_classified() {
        assert!(!RedisEvent::FetchKeys.is_write());
        assert!(!RedisEvent::FetchValue.is_write());
        assert!(!RedisEvent::SelectDb(3).is_write());
        assert!(!RedisEvent::CountMatches {
            pattern: "user:*".into()
        }
        .is_write());
        assert!(!RedisEvent::FetchFullValue {
            key: "k".into(),
            r_type: RedisType::Hash
        }
        .is_write());
        assert!(RedisEvent::DeleteKey { key: "k".into() }.is_write());
        assert!(RedisEvent::SetTtl {
            key: "k".into(),
            secs: 5
        }
        .is_write());
        assert!(RedisEvent::SetString {
            key: "k".into(),
            value: "v".into()
        }
        .is_write());
        assert!(RedisEvent::DeleteItem {
            key: "k".into(),
            delete: ItemDelete::HashField("f".into())
        }
        .is_write());
        assert!(!RedisEvent::FetchPubSubChannels.is_write());
        assert!(RedisEvent::Publish {
            channel: "c".into(),
            payload: "p".into()
        }
        .is_write());
    }
}
