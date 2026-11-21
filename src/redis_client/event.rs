use super::{
    escape::format_cmd,
    types::{ItemDelete, ItemEdit, KeyItem, MemberKind, NewKeySpec, RedisType, Ttl},
    value_edit::ValueEdit,
    write_cmd,
};

/// `Debug` is written by hand (below): events are logged, and a value, a
/// collection element or a Pub/Sub payload must never reach the log file.
#[derive(Clone)]
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
    /// Set a key-level TTL (`EXPIRE`, `0` deletes the key) or clear it
    /// (`PERSIST`).
    SetTtl {
        key: String,
        ttl: Ttl,
    },
    /// Move a key's TTL by `delta_secs`: `TTL` read under `WATCH`, then
    /// `EXPIRE key (ttl + delta)` in a `MULTI`, so a concurrent change is
    /// caught. Refused when the key has no TTL or the result is not
    /// positive.
    ShiftTtl {
        key: String,
        delta_secs: i64,
    },
    /// `RENAMENX from to`; `RENAME` once replacing `to` was confirmed.
    /// `RENAMENX` finding `to` answers `Action::TargetExists`.
    RenameKey {
        from: String,
        to: String,
        overwrite: bool,
    },
    /// `COPY from to [DB n] [REPLACE]`, or before Redis 6.2 `DUMP` +
    /// `RESTORE` with the source's TTL. `db: None` copies within the DB.
    CopyKey {
        from: String,
        to: String,
        db: Option<i64>,
        overwrite: bool,
    },
    /// Rename a HASH field (one script, refused if `to` exists).
    RenameField {
        key: String,
        from: String,
        to: String,
    },
    /// Rename a SET / ZSET member, keeping a ZSET score (one script,
    /// refused if `to` exists).
    RenameMember {
        key: String,
        kind: MemberKind,
        from: String,
        to: String,
    },
    /// `UNLINK` marked keys, [`write_cmd::DELETE_CHUNK`] per command.
    DeleteKeys {
        keys: Vec<String>,
    },
    /// A hash / list / set / zset edited whole in `$EDITOR`: the writes that
    /// make Redis match it, in one `MULTI` (a list keeps its TTL).
    ApplyValueEdit {
        key: String,
        edit: ValueEdit,
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
    /// `SLOWLOG GET` and its two settings into `SharedState::slowlog`.
    FetchSlowlog,
    /// `SLOWLOG RESET`.
    SlowlogReset,
    /// `XINFO CONSUMERS` and `XPENDING` of a group into
    /// `SharedState::stream_group`.
    FetchStreamGroup {
        key: String,
        group: String,
    },
    /// `PUBLISH channel payload`. The payload travels in the event, not in an
    /// `Action`, so it isn't debug-logged as one.
    Publish {
        channel: String,
        payload: String,
    },
    /// `SPUBLISH channel payload` (Redis 7+). Like [`Self::Publish`], the
    /// payload never reaches an `Action`.
    SPublish {
        channel: String,
        payload: String,
    },
}

/// Prints the variant and the key (or channel, pattern, DB); values,
/// elements and payloads only as their size.
impl std::fmt::Debug for RedisEvent {
    #[allow(clippy::too_many_lines)] // one arm per variant
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn bytes(text: &str) -> String {
            format!("<{} bytes>", text.len())
        }
        match self {
            Self::FetchKeys => f.write_str("FetchKeys"),
            Self::BackfillMeta => f.write_str("BackfillMeta"),
            Self::LoadAll { until } => write!(f, "LoadAll {{ until: {until:?} }}"),
            Self::StopLoadAll => f.write_str("StopLoadAll"),
            Self::SelectDb(db) => write!(f, "SelectDb({db})"),
            Self::FetchValue => f.write_str("FetchValue"),
            Self::CountMatches { pattern } => {
                write!(f, "CountMatches {{ pattern: {pattern:?} }}")
            }
            Self::FetchFullValue { key, r_type } => {
                write!(f, "FetchFullValue {{ key: {key:?}, r_type: {r_type:?} }}")
            }
            Self::SetString { key, value } => {
                write!(f, "SetString {{ key: {key:?}, value: {} }}", bytes(value))
            }
            Self::DeleteKey { key } => write!(f, "DeleteKey {{ key: {key:?} }}"),
            Self::SetTtl { key, ttl } => {
                write!(f, "SetTtl {{ key: {key:?}, ttl: {ttl:?} }}")
            }
            Self::ShiftTtl { key, delta_secs } => {
                write!(f, "ShiftTtl {{ key: {key:?}, delta_secs: {delta_secs} }}")
            }
            Self::RenameKey {
                from,
                to,
                overwrite,
            } => write!(
                f,
                "RenameKey {{ from: {from:?}, to: {to:?}, overwrite: {overwrite} }}"
            ),
            Self::CopyKey {
                from,
                to,
                db,
                overwrite,
            } => write!(
                f,
                "CopyKey {{ from: {from:?}, to: {to:?}, db: {db:?}, overwrite: {overwrite} }}"
            ),
            // Fields and members are data: sizes only.
            Self::RenameField { key, from, to } => write!(
                f,
                "RenameField {{ key: {key:?}, from: {}, to: {} }}",
                bytes(from),
                bytes(to)
            ),
            Self::RenameMember {
                key,
                kind,
                from,
                to,
            } => write!(
                f,
                "RenameMember {{ key: {key:?}, kind: {kind:?}, from: {}, to: {} }}",
                bytes(from),
                bytes(to)
            ),
            Self::DeleteKeys { keys } => write!(f, "DeleteKeys {{ <{} keys> }}", keys.len()),
            Self::ApplyValueEdit { key, edit } => write!(
                f,
                "ApplyValueEdit {{ key: {key:?}, <{} changes> }}",
                edit.changes()
            ),
            Self::CreateKey { key, spec } => {
                let (kind, len) = match spec {
                    NewKeySpec::String(_) => ("String", 1),
                    NewKeySpec::List(items) => ("List", items.len()),
                    NewKeySpec::Set(items) => ("Set", items.len()),
                    NewKeySpec::Hash(pairs) => ("Hash", pairs.len()),
                    NewKeySpec::Zset(pairs) => ("Zset", pairs.len()),
                };
                write!(
                    f,
                    "CreateKey {{ key: {key:?}, spec: {kind} <{len} elements> }}"
                )
            }
            Self::AddItem { key, item } => {
                let kind = match item {
                    KeyItem::ListValue(_) => "ListValue",
                    KeyItem::SetMember(_) => "SetMember",
                    KeyItem::HashField { .. } => "HashField",
                    KeyItem::ZsetMember { .. } => "ZsetMember",
                };
                write!(f, "AddItem {{ key: {key:?}, item: {kind} }}")
            }
            Self::EditItem { key, edit } => {
                let kind = match edit {
                    ItemEdit::ListSet { .. } => "ListSet",
                    ItemEdit::SetReplace { .. } => "SetReplace",
                    ItemEdit::HashSet { .. } => "HashSet",
                    ItemEdit::ZsetScore { .. } => "ZsetScore",
                };
                write!(f, "EditItem {{ key: {key:?}, edit: {kind} }}")
            }
            Self::DeleteItem { key, delete } => {
                let kind = match delete {
                    ItemDelete::ListIndex { .. } => "ListIndex",
                    ItemDelete::SetMember(_) => "SetMember",
                    ItemDelete::HashField(_) => "HashField",
                    ItemDelete::ZsetMember(_) => "ZsetMember",
                };
                write!(f, "DeleteItem {{ key: {key:?}, delete: {kind} }}")
            }
            Self::FetchPubSubChannels => f.write_str("FetchPubSubChannels"),
            Self::FetchSlowlog => f.write_str("FetchSlowlog"),
            Self::SlowlogReset => f.write_str("SlowlogReset"),
            Self::FetchStreamGroup { key, group } => {
                write!(f, "FetchStreamGroup {{ key: {key:?}, group: {group:?} }}")
            }
            Self::Publish { channel, payload } => write!(
                f,
                "Publish {{ channel: {channel:?}, payload: {} }}",
                bytes(payload)
            ),
            Self::SPublish { channel, payload } => write!(
                f,
                "SPublish {{ channel: {channel:?}, payload: {} }}",
                bytes(payload)
            ),
        }
    }
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
            | Self::FetchPubSubChannels
            | Self::FetchSlowlog
            | Self::FetchStreamGroup { .. } => false,
            Self::SetString { .. }
            | Self::DeleteKey { .. }
            | Self::SetTtl { .. }
            | Self::CreateKey { .. }
            | Self::AddItem { .. }
            | Self::EditItem { .. }
            | Self::DeleteItem { .. }
            | Self::Publish { .. }
            | Self::SPublish { .. }
            | Self::ShiftTtl { .. }
            | Self::RenameKey { .. }
            | Self::CopyKey { .. }
            | Self::RenameField { .. }
            | Self::RenameMember { .. }
            | Self::DeleteKeys { .. }
            | Self::SlowlogReset
            | Self::ApplyValueEdit { .. } => true,
        }
    }
}

impl RedisEvent {
    /// True for writes that remove data: a key or a collection element,
    /// `EXPIRE … 0`, or a rename / copy that replaces an existing key.
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
            | Self::FetchSlowlog
            | Self::FetchStreamGroup { .. }
            | Self::SetString { .. }
            | Self::CreateKey { .. }
            | Self::AddItem { .. }
            | Self::EditItem { .. }
            | Self::Publish { .. }
            | Self::SPublish { .. }
            | Self::ShiftTtl { .. }
            | Self::RenameField { .. }
            | Self::RenameMember { .. } => false,
            Self::SetTtl { ttl, .. } => *ttl == Ttl::Expire(0),
            Self::RenameKey { overwrite, .. } | Self::CopyKey { overwrite, .. } => *overwrite,
            Self::DeleteKey { .. }
            | Self::DeleteItem { .. }
            | Self::DeleteKeys { .. }
            | Self::SlowlogReset => true,
            Self::ApplyValueEdit { edit, .. } => edit.removes(),
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
            | Self::FetchPubSubChannels
            | Self::FetchSlowlog
            | Self::FetchStreamGroup { .. } => return Vec::new(),
            // No argument to quote: read as it is typed.
            Self::SlowlogReset => return vec!["SLOWLOG RESET".to_owned()],
            Self::SetString { key, value } => write_cmd::set_string(key, value),
            Self::DeleteKey { key } => write_cmd::delete_key(key),
            Self::SetTtl { key, ttl } => write_cmd::set_ttl(key, *ttl),
            Self::RenameKey {
                from,
                to,
                overwrite,
            } => vec![write_cmd::rename_key(from, to, *overwrite)],
            Self::CopyKey {
                from,
                to,
                db,
                overwrite,
            } => vec![write_cmd::copy_key(from, to, *db, *overwrite)],
            Self::RenameField { key, from, to } => vec![write_cmd::rename_field(key, from, to)],
            Self::RenameMember {
                key,
                kind,
                from,
                to,
            } => vec![write_cmd::rename_member(key, *kind, from, to)],
            Self::ShiftTtl { key, delta_secs } => return shift_ttl_preview(key, *delta_secs),
            Self::DeleteKeys { keys } => return delete_keys_preview(keys),
            Self::ApplyValueEdit { key, edit } => return value_edit_preview(key, edit),
            Self::CreateKey { key, spec } => write_cmd::create_key(key, spec),
            Self::AddItem { key, item } => write_cmd::add_item(key, item),
            Self::EditItem { key, edit } => write_cmd::edit_item(key, edit),
            Self::DeleteItem { key, delete } => write_cmd::delete_item(key, delete),
            Self::Publish { channel, payload } => write_cmd::publish(channel, payload),
            Self::SPublish { channel, payload } => write_cmd::spublish(channel, payload),
        };
        let mut lines: Vec<String> = cmds.iter().map(format_cmd).collect();
        // Several commands run as one transaction (`client::run_writes`).
        if lines.len() > 1 {
            lines.insert(0, "MULTI".to_owned());
            lines.push("EXEC".to_owned());
        }
        lines
    }

    /// Lines shown under the commands of a preview: what the commands keep
    /// or how they degrade. Empty for most writes.
    ///
    /// No `_` arm on purpose: a new variant must say whether it needs one.
    #[must_use]
    pub fn preview_notes(&self) -> Vec<String> {
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
            | Self::FetchSlowlog
            | Self::FetchStreamGroup { .. }
            | Self::SetString { .. }
            | Self::DeleteKey { .. }
            | Self::CreateKey { .. }
            | Self::AddItem { .. }
            | Self::EditItem { .. }
            | Self::DeleteItem { .. }
            | Self::Publish { .. }
            | Self::SPublish { .. }
            | Self::SetTtl { .. }
            | Self::ShiftTtl { .. }
            | Self::SlowlogReset
            | Self::DeleteKeys { .. } => Vec::new(),
            Self::ApplyValueEdit { edit, .. } => match edit {
                ValueEdit::List { .. } => vec![
                    "the list is replaced whole and keeps its TTL; changes others made since it \
                     was opened are lost"
                        .to_owned(),
                ],
                _ => vec!["only the elements you changed are written".to_owned()],
            },
            Self::RenameKey { .. } => vec!["RENAME keeps the TTL".to_owned()],
            Self::CopyKey { .. } => vec![
                "COPY keeps the TTL; before Redis 6.2: DUMP + RESTORE with the TTL read first"
                    .to_owned(),
            ],
            Self::RenameField { key, from, to } => rename_notes(key, None, from, to),
            Self::RenameMember {
                key,
                kind,
                from,
                to,
            } => rename_notes(key, Some(*kind), from, to),
        }
    }
}

/// Bulk delete keys shown in a preview before `…and N more`.
pub const PREVIEW_KEYS: usize = 10;

/// `UNLINK` with the first [`PREVIEW_KEYS`] keys, one per line, then
/// `…and N more` and how many commands run.
fn delete_keys_preview(keys: &[String]) -> Vec<String> {
    use super::escape::quote_arg;
    let mut lines = vec!["UNLINK".to_owned()];
    for key in keys.iter().take(PREVIEW_KEYS) {
        lines.push(format!(
            "  {}",
            quote_arg(&super::bytes::bytes_from_text(key))
        ));
    }
    if keys.len() > PREVIEW_KEYS {
        lines.push(format!("  …and {} more", keys.len() - PREVIEW_KEYS));
    }
    let chunks = keys.len().div_ceil(write_cmd::DELETE_CHUNK);
    if chunks > 1 {
        lines.push(format!(
            "({chunks} UNLINK commands of up to {} keys)",
            write_cmd::DELETE_CHUNK
        ));
    }
    lines
}

/// Command lines of a preview before `…and N more commands`.
const PREVIEW_COMMANDS: usize = 12;
/// Arguments of one command line shown before `…(+N more)`.
const PREVIEW_ARGS: usize = 9;

/// The writes of a whole-value edit, each line cut short, inside
/// `MULTI` / `EXEC`.
fn value_edit_preview(key: &str, edit: &ValueEdit) -> Vec<String> {
    use super::escape::format_cmd_brief;
    let cmds = write_cmd::apply_value_edit(key, edit);
    let mut lines = vec!["MULTI".to_owned()];
    lines.extend(
        cmds.iter()
            .take(PREVIEW_COMMANDS)
            .map(|cmd| format_cmd_brief(cmd, PREVIEW_ARGS)),
    );
    if cmds.len() > PREVIEW_COMMANDS {
        lines.push(format!(
            "…and {} more commands",
            cmds.len() - PREVIEW_COMMANDS
        ));
    }
    if matches!(edit, ValueEdit::List { .. }) {
        let mut pexpire = redis::cmd("PEXPIRE");
        pexpire.arg(super::bytes::Raw(key));
        lines.push(format!(
            "{} <PTTL>   (if it had a TTL)",
            format_cmd(&pexpire)
        ));
    }
    lines.push("EXEC".to_owned());
    lines
}

/// `WATCH` / `TTL` / `MULTI` / `EXPIRE key <TTL ± n>` / `EXEC`.
fn shift_ttl_preview(key: &str, delta_secs: i64) -> Vec<String> {
    let cmd = |name: &str| {
        let mut cmd = redis::cmd(name);
        cmd.arg(super::bytes::Raw(key));
        format_cmd(&cmd)
    };
    let sign = if delta_secs < 0 { '-' } else { '+' };
    vec![
        cmd("WATCH"),
        cmd("TTL"),
        "MULTI".to_owned(),
        format!(
            "{} <TTL {sign} {}>",
            cmd("EXPIRE"),
            delta_secs.unsigned_abs()
        ),
        "EXEC".to_owned(),
    ]
}

/// The plain commands a rename script stands for, and that they run one by
/// one if the server refuses `EVAL`.
fn rename_notes(key: &str, kind: Option<MemberKind>, from: &str, to: &str) -> Vec<String> {
    let mut notes = vec!["the script does, atomically:".to_owned()];
    notes.extend(
        write_cmd::rename_item_plain(key, kind, from, to)
            .into_iter()
            .map(|line| format!("  {line}")),
    );
    notes.push("if EVAL is refused, these run one by one (not atomic)".to_owned());
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// #70: events are logged with `{:?}`; no value, element or payload
    /// may show up there.
    #[test]
    fn debug_output_holds_no_values() {
        const SECRET: &str = "s3cr3t-value";
        let k = || "user:1".to_owned();
        let secret = || SECRET.to_owned();
        let events = [
            RedisEvent::SetString {
                key: k(),
                value: secret(),
            },
            RedisEvent::CreateKey {
                key: k(),
                spec: NewKeySpec::Hash(vec![(secret(), secret())]),
            },
            RedisEvent::CreateKey {
                key: k(),
                spec: NewKeySpec::String(secret()),
            },
            RedisEvent::AddItem {
                key: k(),
                item: KeyItem::HashField {
                    field: secret(),
                    value: secret(),
                },
            },
            RedisEvent::EditItem {
                key: k(),
                edit: ItemEdit::SetReplace {
                    old: secret(),
                    new: secret(),
                },
            },
            RedisEvent::DeleteItem {
                key: k(),
                delete: ItemDelete::SetMember(secret()),
            },
            RedisEvent::Publish {
                channel: "news".into(),
                payload: secret(),
            },
            RedisEvent::SPublish {
                channel: "news".into(),
                payload: secret(),
            },
            RedisEvent::RenameField {
                key: k(),
                from: secret(),
                to: secret(),
            },
            RedisEvent::RenameMember {
                key: k(),
                kind: MemberKind::Zset,
                from: secret(),
                to: secret(),
            },
        ];
        for event in events {
            let shown = format!("{event:?}");
            assert!(!shown.contains(SECRET), "{shown}");
            assert!(
                shown.contains("user:1") || shown.contains("news"),
                "{shown}"
            );
        }
        assert_eq!(
            format!(
                "{:?}",
                RedisEvent::SetString {
                    key: k(),
                    value: secret()
                }
            ),
            r#"SetString { key: "user:1", value: <12 bytes> }"#
        );
    }

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
                vec![r#"SET "user:1" "hello world" "KEEPTTL""#],
            ),
            (RedisEvent::DeleteKey { key: k() }, vec![r#"DEL "user:1""#]),
            (
                RedisEvent::SetTtl {
                    key: k(),
                    ttl: Ttl::Expire(60),
                },
                vec![r#"EXPIRE "user:1" "60""#],
            ),
            (
                RedisEvent::SetTtl {
                    key: k(),
                    ttl: Ttl::Persist,
                },
                vec![r#"PERSIST "user:1""#],
            ),
            (
                RedisEvent::CreateKey {
                    key: k(),
                    spec: NewKeySpec::String("v".into()),
                },
                vec![r#"SET "user:1" "v" "NX""#],
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
                        expected: "old".into(),
                        value: "v".into(),
                    },
                },
                vec![
                    r#"EVAL "-- LSET if the element is unchanged\nif redis.call('LINDEX', KEYS"…(162 B more) "1" "user:1" "2" "old" "v""#,
                ],
            ),
            (
                RedisEvent::EditItem {
                    key: k(),
                    edit: ItemEdit::SetReplace {
                        old: "a".into(),
                        new: "b".into(),
                    },
                },
                vec![
                    "MULTI",
                    r#"SREM "user:1" "a""#,
                    r#"SADD "user:1" "b""#,
                    "EXEC",
                ],
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
                    delete: ItemDelete::ListIndex {
                        index: 3,
                        expected: "old".into(),
                    },
                },
                vec![
                    r#"EVAL "-- LSET tombstone + LREM if the element is unchanged\nif redis.ca"…(219 B more) "1" "user:1" "3" "old" "__rrover_deleted__\x00""#,
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
            (RedisEvent::SlowlogReset, vec!["SLOWLOG RESET"]),
            (
                RedisEvent::SPublish {
                    channel: "orders".into(),
                    payload: "hi".into(),
                },
                vec![r#"SPUBLISH "orders" "hi""#],
            ),
        ];
        for (event, lines) in cases {
            assert!(event.is_write(), "{event:?}");
            assert_eq!(event.preview(), lines, "{event:?}");
        }
    }

    /// The power edits (sol 08): exact lines, notes, and classification.
    #[test]
    #[allow(clippy::too_many_lines)] // one row per write shape
    fn power_edits_preview_their_commands() {
        let k = || "user:1".to_owned();
        let cases: Vec<(RedisEvent, Vec<&str>, bool)> = vec![
            (
                RedisEvent::RenameKey {
                    from: k(),
                    to: "user:9".into(),
                    overwrite: false,
                },
                vec![r#"RENAMENX "user:1" "user:9""#],
                false,
            ),
            (
                RedisEvent::RenameKey {
                    from: k(),
                    to: "user:9".into(),
                    overwrite: true,
                },
                vec![r#"RENAME "user:1" "user:9""#],
                true,
            ),
            (
                RedisEvent::CopyKey {
                    from: "cart:1".into(),
                    to: "cart:1".into(),
                    db: Some(2),
                    overwrite: false,
                },
                vec![r#"COPY "cart:1" "cart:1" "DB" "2""#],
                false,
            ),
            (
                RedisEvent::CopyKey {
                    from: "cart:1".into(),
                    to: "cart:2".into(),
                    db: None,
                    overwrite: true,
                },
                vec![r#"COPY "cart:1" "cart:2" "REPLACE""#],
                true,
            ),
            (
                RedisEvent::SetTtl {
                    key: k(),
                    ttl: Ttl::Expire(0),
                },
                vec![r#"EXPIRE "user:1" "0""#],
                true,
            ),
            (
                RedisEvent::ShiftTtl {
                    key: k(),
                    delta_secs: 1800,
                },
                vec![
                    r#"WATCH "user:1""#,
                    r#"TTL "user:1""#,
                    "MULTI",
                    r#"EXPIRE "user:1" <TTL + 1800>"#,
                    "EXEC",
                ],
                false,
            ),
            (
                RedisEvent::ShiftTtl {
                    key: k(),
                    delta_secs: -300,
                },
                vec![
                    r#"WATCH "user:1""#,
                    r#"TTL "user:1""#,
                    "MULTI",
                    r#"EXPIRE "user:1" <TTL - 300>"#,
                    "EXEC",
                ],
                false,
            ),
            (
                RedisEvent::DeleteKeys {
                    keys: vec!["a".into(), "b".into()],
                },
                vec!["UNLINK", r#"  "a""#, r#"  "b""#],
                true,
            ),
        ];
        for (event, lines, destructive) in cases {
            assert!(event.is_write(), "{event:?}");
            assert_eq!(event.is_destructive(), destructive, "{event:?}");
            assert_eq!(event.preview(), lines, "{event:?}");
        }

        let rename = RedisEvent::RenameKey {
            from: k(),
            to: "user:9".into(),
            overwrite: false,
        };
        assert_eq!(rename.preview_notes(), ["RENAME keeps the TTL"]);

        let field = RedisEvent::RenameField {
            key: k(),
            from: "name".into(),
            to: "full_name".into(),
        };
        let preview = field.preview();
        assert_eq!(preview.len(), 1);
        assert!(preview[0].starts_with(r#"EVAL "-- rename a hash field"#));
        assert!(preview[0].ends_with(r#" "1" "user:1" "name" "full_name""#));
        assert_eq!(
            field.preview_notes(),
            [
                "the script does, atomically:",
                r#"  HGET "user:1" "name""#,
                r#"  HSETNX "user:1" "full_name" <value>"#,
                r#"  HDEL "user:1" "name""#,
                "if EVAL is refused, these run one by one (not atomic)",
            ]
        );
        assert!(field.is_write() && !field.is_destructive());

        let member = RedisEvent::RenameMember {
            key: "board".into(),
            kind: MemberKind::Zset,
            from: "ada".into(),
            to: "ada_l".into(),
        };
        assert!(member.preview()[0].starts_with(r#"EVAL "-- rename a zset member"#));
        assert!(member
            .preview_notes()
            .contains(&r#"  ZADD "board" NX <score> "ada_l""#.to_owned()));
        let set = RedisEvent::RenameMember {
            key: "tags".into(),
            kind: MemberKind::Set,
            from: "a".into(),
            to: "b".into(),
        };
        assert!(set.preview()[0].starts_with(r#"EVAL "-- rename a set member"#));
        assert!(member.is_write() && set.is_write());
    }

    /// A whole-value edit: brief lines in one `MULTI`, a list's TTL line,
    /// sizes only in `Debug`.
    #[test]
    fn a_whole_value_edit_previews_its_transaction() {
        let hash = RedisEvent::ApplyValueEdit {
            key: "u".into(),
            edit: ValueEdit::Hash {
                set: vec![("name".into(), "s3cr3t".into())],
                del: vec!["old".into()],
            },
        };
        assert_eq!(
            hash.preview(),
            [
                "MULTI",
                r#"HSET "u" "name" "s3cr3t""#,
                r#"HDEL "u" "old""#,
                "EXEC"
            ]
        );
        assert!(hash.is_write() && hash.is_destructive());
        assert_eq!(
            format!("{hash:?}"),
            r#"ApplyValueEdit { key: "u", <2 changes> }"#
        );

        let items: Vec<String> = (0..1200).map(|i| format!("i{i}")).collect();
        let list = RedisEvent::ApplyValueEdit {
            key: "q".into(),
            edit: ValueEdit::List { items },
        };
        let lines = list.preview();
        assert_eq!(lines[1], r#"DEL "q""#);
        assert!(lines[2].starts_with(r#"RPUSH "q" "i0""#));
        assert!(lines[2].ends_with("…(+492 more)"), "{}", lines[2]);
        assert_eq!(
            lines.len(),
            1 + 1 + 3 + 1 + 1,
            "MULTI DEL 3×RPUSH PEXPIRE EXEC"
        );
        assert!(lines[5].starts_with(r#"PEXPIRE "q" <PTTL>"#));
        assert!(list.preview_notes()[0].contains("replaced whole"));

        let add_only = RedisEvent::ApplyValueEdit {
            key: "s".into(),
            edit: ValueEdit::Set {
                add: vec!["a".into()],
                remove: vec![],
            },
        };
        assert!(!add_only.is_destructive());
    }

    /// Up to ten keys, then `…and N more`; more than one chunk says so.
    #[test]
    fn bulk_delete_previews_ten_keys_and_chunks_by_500() {
        let keys = |n: usize| -> Vec<String> { (0..n).map(|i| format!("k{i}")).collect() };
        let preview = RedisEvent::DeleteKeys { keys: keys(47) }.preview();
        assert_eq!(preview.len(), 1 + 10 + 1);
        assert_eq!(preview[10], r#"  "k9""#);
        assert_eq!(preview[11], "  …and 37 more");

        let preview = RedisEvent::DeleteKeys { keys: keys(1234) }.preview();
        assert_eq!(
            preview.last().unwrap(),
            "(3 UNLINK commands of up to 500 keys)"
        );

        for (n, chunks) in [(1, 1), (499, 1), (500, 1), (501, 2), (1234, 3)] {
            let cmds = write_cmd::delete_keys(&keys(n));
            assert_eq!(cmds.len(), chunks, "{n} keys");
            let sizes: Vec<usize> = cmds.iter().map(|c| c.args_iter().count() - 1).collect();
            assert_eq!(sizes.iter().sum::<usize>(), n);
            assert!(sizes.iter().all(|&s| s <= write_cmd::DELETE_CHUNK));
            assert_eq!(write_cmd::delete_keys_del(&keys(n)).len(), chunks);
        }
        assert!(write_cmd::delete_keys(&[]).is_empty());
        assert_eq!(
            format!("{:?}", RedisEvent::DeleteKeys { keys: keys(3) }),
            "DeleteKeys { <3 keys> }"
        );
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
            ttl: Ttl::Expire(5)
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
