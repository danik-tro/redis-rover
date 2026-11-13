use super::{
    escape::format_cmd,
    types::{ItemDelete, ItemEdit, KeyItem, NewKeySpec, RedisType},
    write_cmd,
};

#[derive(Clone, Debug)]
pub enum RedisEvent {
    FetchKeys,
    /// Fetch `TYPE` / `TTL` / `MEMORY USAGE` of loaded keys that lack them,
    /// for a type / TTL / size sort. Runs in its own task, one pipeline of
    /// 1,000 keys at a time; a second request while it runs is ignored.
    BackfillMeta,
    /// Sweep the rest of the keyspace in its own task (`A`), or, with
    /// `until`, only until that key is loaded (`g o`). Sends
    /// `LoadAllProgress` a few times a second, then `LoadAllDone`.
    LoadAll {
        until: Option<String>,
    },
    /// Stop a running `LoadAll`; the keys loaded so far stay.
    StopLoadAll,
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
    #[must_use]
    pub fn is_write(&self) -> bool {
        match self {
            Self::FetchKeys
            | Self::BackfillMeta
            | Self::LoadAll { .. }
            | Self::StopLoadAll
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

impl RedisEvent {
    /// True for writes that remove data: a key or a collection element.
    /// `safety.preview = "destructive"` previews only these.
    ///
    /// No `_` arm on purpose: a new variant must pick a side.
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        match self {
            Self::FetchKeys
            | Self::BackfillMeta
            | Self::LoadAll { .. }
            | Self::StopLoadAll
            | Self::FetchValue
            | Self::SelectDb(_)
            | Self::CountMatches { .. }
            | Self::FetchFullValue { .. }
            | Self::FetchPubSubChannels
            | Self::SetString { .. }
            | Self::SetTtl { .. }
            | Self::CreateKey { .. }
            | Self::AddItem { .. }
            | Self::EditItem { .. }
            | Self::Publish { .. } => false,
            Self::DeleteKey { .. } | Self::DeleteItem { .. } => true,
        }
    }

    /// The Redis commands this event sends, one line each, as `redis-cli`
    /// would show them (`HSET "user:1" "name" "Ada"`). Reads return
    /// nothing. Built from [`write_cmd`], the same commands the runner
    /// sends.
    ///
    /// No `_` arm on purpose: a new variant must say what it sends.
    #[must_use]
    pub fn preview(&self) -> Vec<String> {
        let cmds = match self {
            Self::FetchKeys
            | Self::BackfillMeta
            | Self::LoadAll { .. }
            | Self::StopLoadAll
            | Self::FetchValue
            | Self::SelectDb(_)
            | Self::CountMatches { .. }
            | Self::FetchFullValue { .. }
            | Self::FetchPubSubChannels => return Vec::new(),
            Self::SetString { key, value } => write_cmd::set_string(key, value),
            Self::DeleteKey { key } => write_cmd::delete_key(key),
            Self::SetTtl { key, secs } => write_cmd::set_ttl(key, *secs),
            Self::CreateKey { key, spec } => write_cmd::create_key(key, spec),
            Self::AddItem { key, item } => write_cmd::add_item(key, item),
            Self::EditItem { key, edit } => write_cmd::edit_item(key, edit),
            Self::DeleteItem { key, delete } => write_cmd::delete_item(key, delete),
            Self::Publish { channel, payload } => write_cmd::publish(channel, payload),
        };
        cmds.iter().map(format_cmd).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Every write variant (and every payload shape) with the exact lines it
    /// previews.
    #[test]
    #[allow(clippy::too_many_lines)] // one row per write shape
    fn preview_shows_the_exact_commands() {
        let k = || "user:1".to_owned();
        let cases: Vec<(RedisEvent, Vec<&str>)> = vec![
            (
                RedisEvent::SetString {
                    key: k(),
                    value: "hello world".into(),
                },
                vec![r#"SET "user:1" "hello world""#],
            ),
            (RedisEvent::DeleteKey { key: k() }, vec![r#"DEL "user:1""#]),
            (
                RedisEvent::SetTtl { key: k(), secs: 60 },
                vec![r#"EXPIRE "user:1" "60""#],
            ),
            (
                RedisEvent::SetTtl { key: k(), secs: 0 },
                vec![r#"PERSIST "user:1""#],
            ),
            (
                RedisEvent::CreateKey {
                    key: k(),
                    spec: NewKeySpec::String("v".into()),
                },
                vec![r#"SET "user:1" "v""#],
            ),
            (
                RedisEvent::CreateKey {
                    key: k(),
                    spec: NewKeySpec::List(vec!["a".into(), "b".into()]),
                },
                vec![r#"RPUSH "user:1" "a" "b""#],
            ),
            (
                RedisEvent::CreateKey {
                    key: k(),
                    spec: NewKeySpec::Set(vec!["a".into()]),
                },
                vec![r#"SADD "user:1" "a""#],
            ),
            (
                RedisEvent::CreateKey {
                    key: k(),
                    spec: NewKeySpec::Hash(vec![("name".into(), "Ada".into())]),
                },
                vec![r#"HMSET "user:1" "name" "Ada""#],
            ),
            (
                RedisEvent::CreateKey {
                    key: k(),
                    spec: NewKeySpec::Zset(vec![("ada".into(), 1.5), ("bob".into(), 2.0)]),
                },
                vec![r#"ZADD "user:1" "1.5" "ada" "2.0" "bob""#],
            ),
            (
                RedisEvent::AddItem {
                    key: k(),
                    item: KeyItem::ListValue("x".into()),
                },
                vec![r#"RPUSH "user:1" "x""#],
            ),
            (
                RedisEvent::AddItem {
                    key: k(),
                    item: KeyItem::SetMember("m".into()),
                },
                vec![r#"SADD "user:1" "m""#],
            ),
            (
                RedisEvent::AddItem {
                    key: k(),
                    item: KeyItem::HashField {
                        field: "name".into(),
                        value: "Ada".into(),
                    },
                },
                vec![r#"HSET "user:1" "name" "Ada""#],
            ),
            (
                RedisEvent::AddItem {
                    key: k(),
                    item: KeyItem::ZsetMember {
                        member: "m".into(),
                        score: -3.25,
                    },
                },
                vec![r#"ZADD "user:1" "-3.25" "m""#],
            ),
            (
                RedisEvent::EditItem {
                    key: k(),
                    edit: ItemEdit::ListSet {
                        index: 2,
                        value: "v".into(),
                    },
                },
                vec![r#"LSET "user:1" "2" "v""#],
            ),
            (
                RedisEvent::EditItem {
                    key: k(),
                    edit: ItemEdit::SetReplace {
                        old: "a".into(),
                        new: "b".into(),
                    },
                },
                vec![r#"SREM "user:1" "a""#, r#"SADD "user:1" "b""#],
            ),
            (
                RedisEvent::EditItem {
                    key: k(),
                    edit: ItemEdit::SetReplace {
                        old: "a".into(),
                        new: "a".into(),
                    },
                },
                vec![],
            ),
            (
                RedisEvent::EditItem {
                    key: k(),
                    edit: ItemEdit::HashSet {
                        field: "name".into(),
                        value: "Ada".into(),
                    },
                },
                vec![r#"HSET "user:1" "name" "Ada""#],
            ),
            (
                RedisEvent::EditItem {
                    key: k(),
                    edit: ItemEdit::ZsetScore {
                        member: "m".into(),
                        score: 42.0,
                    },
                },
                vec![r#"ZADD "user:1" "42.0" "m""#],
            ),
            (
                RedisEvent::DeleteItem {
                    key: k(),
                    delete: ItemDelete::ListIndex(3),
                },
                vec![
                    r#"LSET "user:1" "3" "__rrover_deleted__\x00""#,
                    r#"LREM "user:1" "1" "__rrover_deleted__\x00""#,
                ],
            ),
            (
                RedisEvent::DeleteItem {
                    key: k(),
                    delete: ItemDelete::SetMember("m".into()),
                },
                vec![r#"SREM "user:1" "m""#],
            ),
            (
                RedisEvent::DeleteItem {
                    key: k(),
                    delete: ItemDelete::HashField("f".into()),
                },
                vec![r#"HDEL "user:1" "f""#],
            ),
            (
                RedisEvent::DeleteItem {
                    key: k(),
                    delete: ItemDelete::ZsetMember("m".into()),
                },
                vec![r#"ZREM "user:1" "m""#],
            ),
            (
                RedisEvent::Publish {
                    channel: "news".into(),
                    payload: "line 1\nline 2".into(),
                },
                vec![r#"PUBLISH "news" "line 1\nline 2""#],
            ),
        ];
        for (event, lines) in cases {
            assert!(event.is_write(), "{event:?}");
            assert_eq!(event.preview(), lines, "{event:?}");
        }
    }

    #[test]
    fn reads_preview_nothing() {
        let reads = [
            RedisEvent::FetchKeys,
            RedisEvent::BackfillMeta,
            RedisEvent::LoadAll { until: None },
            RedisEvent::LoadAll {
                until: Some("k".into()),
            },
            RedisEvent::StopLoadAll,
            RedisEvent::FetchValue,
            RedisEvent::SelectDb(1),
            RedisEvent::CountMatches {
                pattern: "*".into(),
            },
            RedisEvent::FetchFullValue {
                key: "k".into(),
                r_type: RedisType::String,
            },
            RedisEvent::FetchPubSubChannels,
        ];
        for event in reads {
            assert!(!event.is_write(), "{event:?}");
            assert!(!event.is_destructive(), "{event:?}");
            assert_eq!(event.preview(), Vec::<String>::new(), "{event:?}");
        }
    }

    #[test]
    fn only_deletes_are_destructive() {
        assert!(RedisEvent::DeleteKey { key: "k".into() }.is_destructive());
        assert!(RedisEvent::DeleteItem {
            key: "k".into(),
            delete: ItemDelete::HashField("f".into())
        }
        .is_destructive());
        assert!(!RedisEvent::SetString {
            key: "k".into(),
            value: "v".into()
        }
        .is_destructive());
    }

    #[test]
    fn reads_and_writes_are_classified() {
        assert!(!RedisEvent::FetchKeys.is_write());
        assert!(!RedisEvent::BackfillMeta.is_write());
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
