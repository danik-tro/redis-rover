//! The state task's power edits (sol 08): rename and copy a key, shift a
//! TTL, rename a field / member, delete marked keys. Like every write they
//! never rescan: the keys they touch are read once and patched into the
//! sweep (`EventHandler::keys_written`).

use crate::{
    action::Action,
    redis_client::{
        bytes::shown,
        client::{self, CopyUnsupported, Moved, Renamed},
        types::MemberKind,
        value_edit::ValueEdit,
    },
};

use super::{EventHandler, Link};

impl EventHandler {
    /// `RENAMENX` / `RENAME`. A taken name answers `TargetExists`, so the
    /// App can ask before replacing it.
    pub(super) async fn rename_key(&mut self, from: String, to: String, overwrite: bool) {
        if !self.same_slot(&from, &to).await {
            return;
        }
        match client::rename_key(self.storage.manager(), &from, &to, overwrite).await {
            Ok(Moved::Done) => {
                self.state.keyspace_state.lock().reveal(to.clone());
                self.keys_written(&[from, to]).await;
            }
            Ok(Moved::TargetExists) => self.action_hook(Action::TargetExists {
                from,
                to,
                db: None,
                copy: false,
            }),
            Err(err) => {
                log::error!("RenameKey failed for {from}: {err:?}");
                self.action_hook(Action::Error(format!(
                    "Failed to rename {}: {err}",
                    shown(&from)
                )));
            }
        }
    }

    /// `COPY`, or `DUMP` + `RESTORE` before Redis 6.2 (remembered once
    /// seen). `db` is the target DB; `None` or the current one copies in
    /// place.
    pub(super) async fn copy_key(
        &mut self,
        from: String,
        to: String,
        db: Option<i64>,
        overwrite: bool,
    ) {
        let current = *self.state.db.lock();
        let db = db.filter(|&db| db != current);
        if db.is_none() && !self.same_slot(&from, &to).await {
            return;
        }
        let result = if self.copy_unsupported {
            Err(Box::new(CopyUnsupported) as _)
        } else {
            client::copy_key(self.storage.manager(), &from, &to, db, overwrite).await
        };
        let result = match result {
            Err(err) if err.is::<CopyUnsupported>() => {
                self.copy_unsupported = true;
                self.copy_by_dump(&from, &to, db, overwrite).await
            }
            other => other,
        };
        match result {
            Ok(Moved::Done) => {
                if db.is_none() {
                    self.state.keyspace_state.lock().reveal(to.clone());
                    self.keys_written(&[to]).await;
                } else if let Some(db) = db {
                    self.action_hook(Action::Notice(format!(
                        "Copied to {} in DB {db}",
                        shown(&to)
                    )));
                }
            }
            Ok(Moved::TargetExists) => self.action_hook(Action::TargetExists {
                from,
                to,
                db,
                copy: true,
            }),
            Err(err) => {
                log::error!("CopyKey failed for {from}: {err:?}");
                self.action_hook(Action::Error(format!(
                    "Failed to copy {}: {err}",
                    shown(&from)
                )));
            }
        }
    }

    async fn copy_by_dump(
        &self,
        from: &str,
        to: &str,
        db: Option<i64>,
        overwrite: bool,
    ) -> Result<Moved, Box<dyn std::error::Error + Send + Sync>> {
        let target = match db {
            Some(db) => self.connect_db(db, Link::Interactive).await?,
            None => self.storage.manager(),
        };
        client::copy_key_dump(self.storage.manager(), target, from, to, overwrite).await
    }

    /// `WATCH` / `TTL` / `MULTI EXPIRE EXEC` on a connection of its own:
    /// `WATCH` would cover every command on the shared one.
    pub(super) async fn shift_ttl(&mut self, key: String, delta_secs: i64) {
        let db = *self.state.db.lock();
        let result = match self.connect_db(db, Link::Interactive).await {
            Ok(manager) => client::shift_ttl(manager, &key, delta_secs).await,
            Err(err) => Err(err),
        };
        match result {
            Ok(_) => self.keys_written(&[key]).await,
            Err(err) => {
                log::error!("ShiftTtl failed for {key}: {err:?}");
                self.action_hook(Action::Error(format!(
                    "Failed to shift the TTL of {}: {err}",
                    shown(&key)
                )));
            }
        }
    }

    /// A HASH field (`kind: None`) or SET / ZSET member renamed in one
    /// script, or with a warning where the server refuses `EVAL`.
    pub(super) async fn rename_item(
        &mut self,
        key: String,
        kind: Option<MemberKind>,
        from: String,
        to: String,
    ) {
        match client::rename_item(self.storage.manager(), &key, kind, &from, &to).await {
            Ok(how) => {
                if how == Renamed::Unscripted {
                    self.action_hook(Action::Notice(
                        "The server refuses EVAL: renamed with plain commands (not atomic)"
                            .to_owned(),
                    ));
                }
                self.action_hook(Action::RequestSelectedValue);
                self.keys_written(&[key]).await;
            }
            Err(err) => {
                let what = if kind.is_none() { "field" } else { "member" };
                log::error!("Renaming a {what} in {key} failed: {err:?}");
                self.action_hook(Action::Error(format!(
                    "Failed to rename the {what} in {}: {err}",
                    shown(&key)
                )));
            }
        }
    }

    /// A collection edited whole in `$EDITOR`: one `MULTI`, then the value
    /// and the key's row are fetched again.
    pub(super) async fn apply_value_edit(&mut self, key: String, edit: ValueEdit) {
        match client::apply_value_edit(self.storage.manager(), &key, &edit).await {
            Ok(()) => {
                self.action_hook(Action::RequestSelectedValue);
                self.keys_written(&[key]).await;
            }
            Err(err) => {
                log::error!("ApplyValueEdit failed for {key}: {err:?}");
                self.action_hook(Action::Error(format!(
                    "Failed to save {}: {err}",
                    shown(&key)
                )));
            }
        }
    }

    /// `UNLINK` in chunks; the sweep drops what is gone in one pass.
    pub(super) async fn delete_keys(&mut self, keys: Vec<String>) {
        match client::delete_keys(self.storage.manager(), &keys).await {
            Ok(removed) => {
                self.state.keyspace_state.lock().remove_keys(&keys);
                self.action_hook(Action::KeysDeleted(removed));
                self.action_hook(Action::KeysChanged);
            }
            Err(err) => {
                log::error!("DeleteKeys failed: {err:?}");
                self.action_hook(Action::Error(format!("Failed to delete the keys: {err}")));
                // Some chunks may have gone through: show what is left.
                self.state.keyspace_state.lock().rescan();
                self.action_hook(Action::KeysChanged);
            }
        }
    }

    /// In cluster mode both names must hash to one slot, or `RENAME` /
    /// `COPY` fail with `CROSSSLOT`; say so up front. `true` outside a
    /// cluster.
    async fn same_slot(&self, from: &str, to: &str) -> bool {
        if !self.cluster_enabled() {
            return true;
        }
        match client::key_slots(self.storage.manager(), from, to).await {
            Ok((a, b)) if a == b => true,
            Ok((a, b)) => {
                self.action_hook(Action::Error(format!(
                    "{} (slot {a}) and {} (slot {b}) are in different cluster slots: \
                     rename / copy can't move a key between slots",
                    shown(from),
                    shown(to)
                )));
                false
            }
            Err(err) => {
                log::warn!("CLUSTER KEYSLOT failed, trying anyway: {err}");
                true
            }
        }
    }

    fn cluster_enabled(&self) -> bool {
        self.state
            .metrics
            .lock()
            .latest_snapshot
            .as_ref()
            .is_some_and(|s| s.cluster_enabled)
    }

    /// Several written keys at once (both names of a rename): one metadata
    /// read, one `KeysChanged`.
    pub(super) async fn keys_written(&self, keys: &[String]) {
        match self.storage.fetch_meta(keys).await {
            Ok(rows) => {
                let mut state = self.state.keyspace_state.lock();
                for (key, row) in keys.iter().zip(&rows) {
                    if row.r_type.name() == "none" {
                        state.remove_key(key);
                    } else {
                        state.upsert_key(row);
                    }
                }
            }
            Err(err) => {
                log::warn!("Reading written keys' metadata failed, rescanning: {err}");
                self.state.keyspace_state.lock().rescan();
            }
        }
        self.action_hook(Action::KeysChanged);
    }
}
