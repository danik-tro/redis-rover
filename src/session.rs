//! `session.json`: where the user was, per connection target. It holds view
//! state only (key names and patterns, never a value, a message or a
//! credential) and losing it costs nothing, so a failure here is only logged.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    redis_client::{
        pubsub_store::{Target as PubSubTarget, TargetKind},
        types::{Sort, SortKey},
    },
    utils::create_private_file,
};

/// The file's format. A file of another version is ignored.
const VERSION: u32 = 1;
/// A larger file isn't one of ours: ignore it rather than read it all.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
const FILE_NAME: &str = "session.json";

/// A sort column, as saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedSortKey {
    Name,
    Type,
    Ttl,
    Size,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSort {
    pub key: SavedSortKey,
    pub descending: bool,
}

impl From<Sort> for SavedSort {
    fn from(sort: Sort) -> Self {
        Self {
            key: match sort.key {
                SortKey::Name => SavedSortKey::Name,
                SortKey::Type => SavedSortKey::Type,
                SortKey::Ttl => SavedSortKey::Ttl,
                SortKey::Size => SavedSortKey::Size,
            },
            descending: sort.descending,
        }
    }
}

impl From<SavedSort> for Sort {
    fn from(saved: SavedSort) -> Self {
        Self {
            key: match saved.key {
                SavedSortKey::Name => SortKey::Name,
                SavedSortKey::Type => SortKey::Type,
                SavedSortKey::Ttl => SortKey::Ttl,
                SavedSortKey::Size => SortKey::Size,
            },
            descending: saved.descending,
        }
    }
}

/// A Pub/Sub subscription's kind and name. No messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SavedSubKind {
    Channel,
    Pattern,
    Shard,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSub {
    pub kind: SavedSubKind,
    pub name: String,
}

impl From<&PubSubTarget> for SavedSub {
    fn from(target: &PubSubTarget) -> Self {
        let kind = match target {
            PubSubTarget::Channel(_) => SavedSubKind::Channel,
            PubSubTarget::Pattern(_) => SavedSubKind::Pattern,
            PubSubTarget::Shard(_) => SavedSubKind::Shard,
        };
        Self {
            kind,
            name: target.name().to_owned(),
        }
    }
}

impl From<SavedSub> for PubSubTarget {
    fn from(saved: SavedSub) -> Self {
        let kind = match saved.kind {
            SavedSubKind::Channel => TargetKind::Channel,
            SavedSubKind::Pattern => TargetKind::Pattern,
            SavedSubKind::Shard => TargetKind::Shard,
        };
        Self::new(kind, saved.name)
    }
}

/// Where the user was on one target.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    pub filter: Option<String>,
    pub types: Vec<String>,
    pub sort: Option<SavedSort>,
    pub tree: bool,
    /// Open tree groups, by prefix.
    pub expanded: Vec<String>,
    pub selected_key: Option<String>,
    pub db: Option<i64>,
    /// Width of the key pane, in percent.
    pub split: Option<u16>,
    pub subscriptions: Vec<SavedSub>,
}

/// The whole file: one state per target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionFile {
    pub version: u32,
    pub targets: BTreeMap<String, SessionState>,
}

impl Default for SessionFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            targets: BTreeMap::new(),
        }
    }
}

/// The session file in `dir`.
#[must_use]
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME)
}

/// Read the file. A missing, oversized, corrupt or other-version file is
/// an empty session.
#[must_use]
pub fn load(path: &Path) -> SessionFile {
    match read(path) {
        Ok(Some(file)) => file,
        Ok(None) => SessionFile::default(),
        Err(err) => {
            log::warn!("ignoring session file {}: {err}", path.display());
            SessionFile::default()
        }
    }
}

fn read(path: &Path) -> io::Result<Option<SessionFile>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Err(io::Error::other("too large"));
    }
    let file: SessionFile = serde_json::from_slice(&fs::read(path)?)?;
    Ok((file.version == VERSION).then_some(file))
}

/// Store `state` under `key`, keeping what other targets saved. The file is
/// private and replaced in one step, so a crash leaves the old one.
///
/// # Errors
///
/// The directory or file can't be written.
pub fn save(path: &Path, key: &str, state: &SessionState) -> io::Result<()> {
    let mut file = load(path);
    file.targets.insert(key.to_owned(), state.clone());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = create_private_file(&temp)
        .and_then(|out| serde_json::to_writer_pretty(out, &file).map_err(io::Error::from))
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// [`save`], logging a failure: a read-only disk must not disturb the app.
#[allow(dead_code)] // Session restore, the next step, is the caller.
pub fn save_logged(path: &Path, key: &str, state: &SessionState) {
    if let Err(err) = save(path, key, state) {
        log::warn!("could not save the session to {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use clap::Parser;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{cli::Cli, target::resolve};

    /// A fresh directory, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("rrover-session-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            path_in(&self.0)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn full_state() -> SessionState {
        SessionState {
            filter: Some("user:*".into()),
            types: vec!["hash".into(), "zset".into()],
            sort: Some(SavedSort {
                key: SavedSortKey::Size,
                descending: true,
            }),
            tree: true,
            expanded: vec!["user:".into(), "user:1:".into()],
            selected_key: Some("user:42".into()),
            db: Some(3),
            split: Some(40),
            subscriptions: vec![
                SavedSub {
                    kind: SavedSubKind::Channel,
                    name: "orders".into(),
                },
                SavedSub {
                    kind: SavedSubKind::Pattern,
                    name: "orders:*".into(),
                },
                SavedSub {
                    kind: SavedSubKind::Shard,
                    name: "shard:1".into(),
                },
            ],
        }
    }

    #[test]
    fn a_session_survives_the_round_trip() {
        let dir = TempDir::new("round-trip");
        let state = full_state();

        save(&dir.file(), "profile:prod", &state).unwrap();

        assert_eq!(load(&dir.file()).targets["profile:prod"], state);
    }

    #[test]
    fn saving_one_target_keeps_the_others() {
        let dir = TempDir::new("targets");
        let other = SessionState {
            filter: Some("other:*".into()),
            ..SessionState::default()
        };
        save(&dir.file(), "a:6379/0", &other).unwrap();

        save(&dir.file(), "b:6379/0", &full_state()).unwrap();

        let file = load(&dir.file());
        assert_eq!(file.targets["a:6379/0"], other);
        assert_eq!(file.targets["b:6379/0"], full_state());
    }

    #[test]
    fn sort_and_subscriptions_convert_both_ways() {
        for key in [SortKey::Name, SortKey::Type, SortKey::Ttl, SortKey::Size] {
            let sort = Sort {
                key,
                descending: true,
            };
            assert_eq!(Sort::from(SavedSort::from(sort)), sort);
        }
        for target in [
            PubSubTarget::Channel("c".into()),
            PubSubTarget::Pattern("p*".into()),
            PubSubTarget::Shard("s".into()),
        ] {
            assert_eq!(PubSubTarget::from(SavedSub::from(&target)), target);
        }
    }

    #[test]
    fn a_missing_or_broken_file_is_an_empty_session() {
        let dir = TempDir::new("broken");
        assert_eq!(load(&dir.file()), SessionFile::default());

        fs::write(dir.file(), "{ not json").unwrap();
        assert_eq!(load(&dir.file()), SessionFile::default());

        fs::write(dir.file(), r#"{"version": 999, "targets": {"a": {}}}"#).unwrap();
        assert_eq!(load(&dir.file()), SessionFile::default());
    }

    #[test]
    fn a_broken_file_is_replaced_by_the_next_save() {
        let dir = TempDir::new("replace");
        fs::write(dir.file(), "{ not json").unwrap();

        save(&dir.file(), "a", &full_state()).unwrap();

        assert_eq!(load(&dir.file()).targets["a"], full_state());
    }

    #[test]
    fn a_failed_save_is_only_an_error_value() {
        let dir = TempDir::new("failed");
        let blocker = dir.0.join("file");
        fs::write(&blocker, "").unwrap();
        let path = blocker.join("session.json");

        assert!(save(&path, "a", &full_state()).is_err());
        save_logged(&path, "a", &full_state());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("private");

        save(&dir.file(), "a", &full_state()).unwrap();

        let mode = fs::metadata(dir.file()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = TempDir::new("temp");

        save(&dir.file(), "a", &full_state()).unwrap();

        let names: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(FILE_NAME)]);
    }

    /// No URL form can put a password or user name into the file.
    #[test]
    fn no_credential_reaches_the_file() {
        let dir = TempDir::new("secret");
        for url in [
            "redis://alice:hunter2@db.example:6380/3",
            "redis://:hunter2@db.example:6380/3",
            "rediss://alice:hunter2@db.example:6380/3",
            "redis-cluster://alice:hunter2@n1.example:7000,n2.example:7001",
            "redis+sentinel://alice:hunter2@s1.example:26379,s2.example/mymaster/3",
        ] {
            let cli = Cli::parse_from(["rrover", "--url", url]);
            let target = resolve(&cli, &HashMap::new(), None).unwrap();
            let key = target.session_key();
            assert!(!key.contains("hunter2"), "{key}");
            assert!(!key.contains("alice"), "{key}");

            save(&dir.file(), &key, &full_state()).unwrap();
            let written = fs::read_to_string(dir.file()).unwrap();
            assert!(!written.contains("hunter2"), "{written}");
            assert!(!written.contains("alice"), "{written}");
        }
    }

    #[test]
    fn the_key_tells_servers_and_dbs_apart() {
        let key = |url: &str| {
            let cli = Cli::parse_from(["rrover", "--url", url]);
            resolve(&cli, &HashMap::new(), None).unwrap().session_key()
        };

        assert_eq!(key("redis://db.example:6380/3"), "db.example:6380/3");
        assert_ne!(key("redis://a:6379/0"), key("redis://b:6379/0"));
        assert_ne!(key("redis://a:6379/0"), key("redis://a:6380/0"));
        assert_ne!(key("redis://a:6379/0"), key("redis://a:6379/1"));
    }

    #[test]
    fn a_profile_is_its_name_unless_the_url_replaces_its_server() {
        let profiles: HashMap<String, crate::target::Profile> =
            serde_json::from_str(r#"{"prod": {"url": "redis://user:hunter2@db.example:6379/0"}}"#)
                .unwrap();

        let cli = Cli::parse_from(["rrover", "prod"]);
        let named = resolve(&cli, &profiles, None).unwrap().session_key();
        assert_eq!(named, "profile:prod");

        let cli = Cli::parse_from(["rrover", "prod", "--url", "redis://other.example:6379/0"]);
        let replaced = resolve(&cli, &profiles, None).unwrap().session_key();
        assert_eq!(replaced, "other.example:6379/0");
    }
}
