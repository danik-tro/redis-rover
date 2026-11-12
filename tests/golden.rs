//! Golden frames: each test draws one screen from fixed data into a
//! `TestBackend`-sized buffer and compares its text (no styles) with
//! `tests/golden/<name>.txt`.
//!
//! After an intended UI change, rewrite the files and review their diff:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test --test golden
//! ```

use std::{
    collections::HashMap,
    fmt::Write as _,
    path::PathBuf,
    sync::{Arc, Once},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{StatefulWidget, Widget},
};
use redis_rover::internals::{
    render_help, Clock, ConfirmPopup, ConnStatus, Dashboard, DashboardWidget, Environment,
    FixedClock, FrameData, Info, InfoSnapshot, InfoWidget, ItemEdit, JsonField, JsonPreview,
    JsonPreviewKind, KeyMeta, KeySpace, KeySpaceWidget, KeyValue, LoadedValue, MetricsHistory,
    Mode, PageInfo, PubSub, PubSubWidget, RedisEvent, RedisInfo, RedisType, SharedState, Target,
    Toast, ToastWidget, ValuePageInfo,
};

static SETUP: Once = Once::new();

/// The bundled config instead of the user's, and UTC so message times in
/// the Pub/Sub frames don't depend on the machine's time zone.
fn init() {
    SETUP.call_once(|| {
        let empty = std::env::temp_dir().join("rrover-test-no-config");
        std::env::set_var("REDIS_ROVER_CONFIG", &empty);
        std::env::set_var("RROVER_CONFIG", &empty);
        std::env::set_var("TZ", "UTC");
    });
}

/// The buffer as text: one line per row, trailing spaces and trailing
/// blank rows trimmed (the end-of-file hook would strip them anyway).
fn dump(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let row: String = (area.left()..area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        out.push_str(row.trim_end());
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Compare `buf` with `tests/golden/<name>.txt`, or rewrite the file when
/// `UPDATE_GOLDEN` is set. A mismatch lists the rows that differ.
fn check(name: &str, buf: &Buffer) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    let actual = dump(buf);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let Ok(expected) = std::fs::read_to_string(&path) else {
        panic!(
            "missing {}; run `UPDATE_GOLDEN=1 cargo test --test golden`",
            path.display()
        );
    };
    if expected == actual {
        return;
    }
    let (want, got): (Vec<&str>, Vec<&str>) =
        (expected.lines().collect(), actual.lines().collect());
    let mut diff = String::new();
    for row in 0..want.len().max(got.len()) {
        let (w, g) = (want.get(row), got.get(row));
        if w != g {
            let _ = writeln!(diff, "row {row}:");
            let _ = writeln!(diff, "- {}", w.unwrap_or(&"<none>"));
            let _ = writeln!(diff, "+ {}", g.unwrap_or(&"<none>"));
        }
    }
    panic!(
        "golden frame `{name}` changed (- expected, + actual):\n{diff}\n\
         If the change is intended: UPDATE_GOLDEN=1 cargo test --test golden"
    );
}

fn buffer(width: u16, height: u16) -> Buffer {
    Buffer::empty(Rect::new(0, 0, width, height))
}

// ---- Keyspace -------------------------------------------------------------

fn key(name: &str, r_type: RedisType, size: u128, ttl: isize) -> KeyMeta {
    KeyMeta {
        key: name.into(),
        r_type,
        size,
        ttl,
        value: KeyValue::Unknown,
    }
}

/// A page of keys of every type, with and without TTL.
fn key_rows() -> Vec<KeyMeta> {
    vec![
        key("user:1001", RedisType::Hash, 312, -1),
        key("user:1002", RedisType::Hash, 296, -1),
        key("session:7f3a9c", RedisType::String, 96, 3_600),
        key("queue:jobs", RedisType::List, 2_048, -1),
        key("tags:popular", RedisType::Set, 540, -1),
        key("leaderboard:2024", RedisType::Zset, 1_200, 86_400),
        key("events:orders", RedisType::Stream, 8_192, -1),
        key("doc:settings", RedisType::Json, 420, -1),
        key("cache:page:/home", RedisType::String, 15_000, 120),
        key(
            "bloom:emails",
            RedisType::Unknown("MBbloom--".into()),
            1_024,
            -1,
        ),
    ]
}

fn page(keys: usize, known_total: usize, pattern: Option<&str>) -> PageInfo {
    PageInfo {
        page: 0,
        start: 0,
        end: keys,
        known_total,
        complete: true,
        page_size: 10,
        scan_cursor: 0,
        pattern: pattern.map(Into::into),
    }
}

fn keyspace(keys: Vec<KeyMeta>, page_info: PageInfo) -> KeySpace {
    // As the App loads a page: keys, then the page they belong to.
    let mut keyspace = KeySpace::new(Vec::new());
    keyspace.set_keys(keys);
    keyspace.set_page_info(page_info);
    keyspace
}

fn draw_keyspace(keyspace: &mut KeySpace, width: u16, height: u16) -> Buffer {
    let mut buf = buffer(width, height);
    KeySpaceWidget.render(buf.area, &mut buf, keyspace);
    buf
}

/// The default key list with `name` selected and its value loaded.
fn with_value(name: &str, value: KeyValue) -> KeySpace {
    let mut keyspace = keyspace(key_rows(), page(10, 10, None));
    assert!(keyspace.select_key_by_name(name), "{name} is in key_rows");
    keyspace.set_selected_value(Some(LoadedValue {
        value,
        page: ValuePageInfo::default(),
    }));
    keyspace
}

#[test]
fn keys_80() {
    init();
    let mut keyspace = keyspace(key_rows(), page(10, 10, None));
    check("keys_80", &draw_keyspace(&mut keyspace, 80, 20));
}

#[test]
fn keys_120() {
    init();
    let mut keyspace = keyspace(key_rows(), page(10, 10, None));
    check("keys_120", &draw_keyspace(&mut keyspace, 120, 20));
}

#[test]
fn keys_filtered() {
    init();
    let users = key_rows()
        .into_iter()
        .filter(|k| k.key.starts_with("user:"))
        .collect::<Vec<_>>();
    let mut keyspace = keyspace(users, page(2, 2, Some("user:*")));
    check("keys_filtered", &draw_keyspace(&mut keyspace, 120, 12));
}

#[test]
fn keys_empty() {
    init();
    let mut keyspace = keyspace(Vec::new(), page(0, 0, Some("nothing:*")));
    check("keys_empty", &draw_keyspace(&mut keyspace, 120, 12));
}

#[test]
fn value_string() {
    init();
    let text = "Hello from redis-rover.\nA second line of a plain string value.";
    let mut keyspace = with_value(
        "session:7f3a9c",
        KeyValue::String {
            value: text.into(),
            total_len: text.len(),
        },
    );
    check("value_string", &draw_keyspace(&mut keyspace, 120, 20));
}

#[test]
fn value_json() {
    init();
    let doc = serde_json::json!({
        "theme": "dark",
        "notifications": { "email": true, "push": false },
        "shortcuts": ["j", "k", "/"],
    });
    let mut keyspace = with_value("doc:settings", KeyValue::Json(doc));
    check("value_json", &draw_keyspace(&mut keyspace, 120, 20));
}

/// #49: a `RedisJSON` document above the preview limit, focused on its field
/// table (page 1 of its top-level fields).
#[test]
fn value_json_preview() {
    init();
    let mut keys = key_rows();
    keys[7].size = 52_428_800;
    let mut keyspace = keyspace(keys, page(10, 10, None));
    assert!(keyspace.select_key_by_name("doc:settings"));
    let field = |name: &str, json_type: &str, bytes: u64| JsonField {
        name: name.into(),
        json_type: json_type.into(),
        bytes,
    };
    let fields = vec![
        field("orders", "array", 41_943_040),
        field("customers", "object", 9_437_184),
        field("schema_version", "integer", 16),
        field("generated at", "string", 48),
    ];
    keyspace.set_selected_value(Some(LoadedValue {
        value: KeyValue::JsonPreview(JsonPreview {
            bytes: 52_428_800,
            kind: JsonPreviewKind::Object {
                len: 1_250,
                fields: fields.clone(),
            },
        }),
        page: ValuePageInfo {
            start: 0,
            len: fields.len(),
            total: Some(1_250),
            has_next: true,
            has_prev: false,
        },
    }));
    keyspace.enter_value();
    check("value_json_preview", &draw_keyspace(&mut keyspace, 120, 20));
}

#[test]
fn value_hash() {
    init();
    let fields = [
        ("name", "Ann Example"),
        ("email", "ann@example.com"),
        ("plan", "pro"),
        ("visits", "42"),
    ];
    let mut keyspace = with_value(
        "user:1001",
        KeyValue::Hash(
            fields
                .iter()
                .map(|(f, v)| ((*f).into(), (*v).into()))
                .collect(),
        ),
    );
    check("value_hash", &draw_keyspace(&mut keyspace, 120, 20));
}

/// AC5: TTLs and a toast drawn at a fixed time render the same on every run.
#[test]
fn keys_ttl_toast() {
    init();
    let clock = FixedClock::new(at(0));
    let mut keyspace = with_value(
        "cache:page:/home",
        KeyValue::String {
            value: "<html>…</html>".into(),
            total_len: 15_000,
        },
    );
    let mut toast = Toast::default();
    toast.show("Copied cache:page:/home (15000 bytes)", false, clock.now());
    // Still up a second later.
    clock.advance(Duration::from_secs(1));
    toast.expire(clock.now());

    let mut buf = draw_keyspace(&mut keyspace, 120, 20);
    ToastWidget.render(buf.area, &mut buf, &mut toast);
    check("keys_ttl_toast", &buf);
}

// ---- Footer ---------------------------------------------------------------

fn sample_info() -> RedisInfo {
    RedisInfo::from(HashMap::from([
        ("redis_version", "7.2.4"),
        ("os", "Linux 6.8.0-1012-aws x86_64"),
        ("used_memory_human", "1.53M"),
        ("total_system_memory_human", "15.47G"),
        ("connected_clients", "3"),
        ("maxclients", "10000"),
        ("db0", "keys=1234,expires=0,avg_ttl=0"),
    ]))
}

/// The footer as `--read-only` on a local server draws it.
#[test]
fn footer_read_only() {
    check("footer_read_only", &footer(Environment::Local, true));
}

/// The environment badge, one frame per environment with its default.
#[test]
fn footer_environments() {
    for (name, env, read_only) in [
        ("footer_local", Environment::Local, false),
        ("footer_staging", Environment::Staging, false),
        ("footer_prod_ro", Environment::Prod, true),
        ("footer_unknown_ro", Environment::Unknown, true),
    ] {
        check(name, &footer(env, read_only));
    }
}

fn footer(env: Environment, read_only: bool) -> Buffer {
    init();
    let shared = SharedState::default();
    *shared.info.lock() = Some(sample_info());
    let mut info = Info::new("redis://localhost:6379");
    info.environment = env;
    info.read_only = read_only;
    info.hint = "o open  y yank  / filter  d db  ? help".into();
    let mut frame = FrameData::default();
    frame.refresh_footer(&shared);

    let mut buf = buffer(100, 5);
    InfoWidget { frame: &frame }.render(buf.area, &mut buf, &mut info);
    buf
}

// ---- Write preview -------------------------------------------------------

/// AC3: editing a hash field previews the exact `HSET`.
#[test]
fn preview_hset() {
    init();
    let lines = RedisEvent::EditItem {
        key: "user:1".into(),
        edit: ItemEdit::HashSet {
            field: "name".into(),
            value: "Ada".into(),
        },
    }
    .preview();
    for (name, w, h) in [("preview_hset_80", 80, 24), ("preview_hset_120", 120, 30)] {
        let mut buf = buffer(w, h);
        ConfirmPopup::write_preview(&lines, Environment::Local).render(buf.area, &mut buf);
        check(name, &buf);
    }
}

/// A long value is cut with its size, the line wraps inside the popup, and
/// composite writes show every command.
#[test]
fn preview_long_and_composite() {
    init();
    let mut lines = RedisEvent::SetString {
        key: "session:7f3a9c".into(),
        value: format!("{{\"user\": \"ada\", \"bio\": \"{}\"}}", "x".repeat(3000)),
    }
    .preview();
    lines.extend(
        RedisEvent::EditItem {
            key: "tags".into(),
            edit: ItemEdit::SetReplace {
                old: "old tag".into(),
                new: "new\ttag".into(),
            },
        }
        .preview(),
    );
    let mut buf = buffer(80, 24);
    ConfirmPopup::write_preview(&lines, Environment::Prod).render(buf.area, &mut buf);
    check("preview_long_80", &buf);
}

// ---- Dashboard ------------------------------------------------------------

/// 30 one-second samples with steady growth.
fn history() -> MetricsHistory {
    let t0 = Instant::now();
    let mut h = MetricsHistory::new(100);
    for i in 0..30_u32 {
        let n = u64::from(i);
        let mut s = InfoSnapshot::empty(t0 + Duration::from_secs(n));
        s.total_commands = Some(n * 1_000);
        s.net_in_bytes = Some(n * 4_096);
        s.net_out_bytes = Some(n * 8_192);
        s.used_memory = Some(1_000_000 + n * 1_000);
        s.maxmemory = Some(10_000_000);
        s.connected_clients = Some(3);
        s.keyspace_hits = Some(n * 90);
        s.keyspace_misses = Some(n * 10);
        s.cpu_user = Some(f64::from(i) * 0.1);
        s.cpu_sys = Some(f64::from(i) * 0.05);
        h.record(s);
    }
    h
}

#[test]
fn dashboard_120() {
    init();
    let dashboard = Dashboard::new(Arc::new(Mutex::new(history())));
    let (width, height) = (120, 30);
    let frame = FrameData {
        metrics: dashboard.live_window(width),
        ..FrameData::default()
    };
    let mut dashboard = dashboard;
    let mut buf = buffer(width, height);
    DashboardWidget { frame: &frame }.render(buf.area, &mut buf, &mut dashboard);
    check("dashboard_120", &buf);
}

// ---- Pub/Sub --------------------------------------------------------------

/// 2023-11-14 22:13:20 UTC.
fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
}

#[test]
fn pubsub_log_paused() {
    init();
    // The store with the default config's buffer limits.
    let store = SharedState::default().pubsub;
    {
        let mut store = store.lock();
        store.status = ConnStatus::Connected;
        store.add(Target::Channel("orders".into()));
        store.add(Target::Pattern("audit:*".into()));
        for i in 0..20 {
            let payload = format!(r#"{{"id":{i},"total":{}}}"#, 10 * i);
            store.push("orders", None, payload.as_bytes(), at(i));
        }
    }
    // Five seconds after the last message: the rates are steady.
    let clock = FixedClock::new(at(25));
    let mut pubsub = PubSub::new(store.clone());
    let area = Rect::new(0, 0, 110, 24);
    // A frame before the keys, as in the app: the log learns its height.
    pubsub.build_view(area, clock.now(), clock.wall());
    pubsub.toggle_focus();
    pubsub.toggle_pause();
    pubsub.log_up();
    pubsub.log_up();
    // A message after the pause shows as `+1 new`.
    store.lock().push("orders", None, br#"{"id":20}"#, at(20));

    let mut buf = Buffer::empty(area);
    let view = pubsub.build_view(area, clock.now(), clock.wall());
    PubSubWidget { view }.render(buf.area, &mut buf, &mut pubsub);
    check("pubsub_log_paused", &buf);
}

// ---- Help -----------------------------------------------------------------

#[test]
fn help_keyspace() {
    init();
    let mut buf = buffer(120, 40);
    render_help(buf.area, &mut buf, Mode::KeySpace);
    check("help_keyspace", &buf);
}
