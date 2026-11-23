//! One key-list frame per theme. The config is read once per process, so
//! each theme is drawn by a child run of this test binary with its own
//! config dir. A frame file holds the text, the attributes of each cell and
//! the colours used.
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test --test golden_themes
//! ```

use std::{collections::BTreeSet, fmt::Write as _, path::PathBuf, process::Command};

use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier},
    widgets::StatefulWidget,
};
use redis_rover::internals::{
    finish_frame, Environment, FrameData, Info, InfoWidget, KeyMeta, KeySpace, KeySpaceWidget,
    KeyValue, Liveness, LoadedValue, PageInfo, RedisType, ValuePageInfo,
};

const CHILD: &str = "RROVER_THEME_FRAME";

fn key(name: &str, r_type: RedisType, ttl: isize) -> KeyMeta {
    KeyMeta {
        key: name.into(),
        r_type,
        size: 128,
        ttl,
        value: KeyValue::Unknown,
    }
}

/// Selection, a mark, liveness, every badge and the environment on one frame.
fn frame() -> Buffer {
    let keys = vec![
        key("user:1001", RedisType::Hash, -1),
        key("session:7f3a", RedisType::String, 3_600),
        key("queue:jobs", RedisType::List, -1),
        key("tags:popular", RedisType::Set, -1),
        key("board:2024", RedisType::Zset, -1),
        key("events:orders", RedisType::Stream, -1),
        key("doc:settings", RedisType::Json, -1),
    ];
    let mut keyspace = KeySpace::new(Vec::new());
    keyspace.set_keys(keys);
    keyspace.set_page_info(PageInfo {
        end: 7,
        known_total: 7,
        complete: true,
        page_size: 10,
        ..PageInfo::default()
    });
    assert!(keyspace.select_key_by_name("queue:jobs"));
    assert!(keyspace.toggle_mark());
    keyspace.set_selected_value(Some(LoadedValue {
        value: KeyValue::List(vec!["first".into(), "second".into()]),
        page: ValuePageInfo::default(),
    }));
    keyspace.set_liveness(Some(Liveness::Live));

    let area = Rect::new(0, 0, 100, 19);
    let mut buf = Buffer::empty(area);
    let [main, footer] = Layout::vertical([Constraint::Fill(1), Constraint::Length(5)]).areas(area);
    KeySpaceWidget.render(main, &mut buf, &mut keyspace);
    let mut info = Info::new("redis://localhost:6379");
    info.environment = Environment::Prod;
    info.read_only = true;
    InfoWidget {
        frame: &FrameData::default(),
    }
    .render(footer, &mut buf, &mut info);
    finish_frame(&mut buf);
    buf
}

fn color_name(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    }
}

fn dump(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let row: String = (area.left()..area.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        let _ = writeln!(out, "{}", row.trim_end());
    }
    out.push_str("--- attributes: R reversed, B bold, # both ---\n");
    for y in area.top()..area.bottom() {
        let row: String = (area.left()..area.right())
            .map(|x| {
                let modifier = buf[(x, y)].modifier;
                match (
                    modifier.contains(Modifier::REVERSED),
                    modifier.contains(Modifier::BOLD),
                ) {
                    (true, true) => '#',
                    (true, false) => 'R',
                    (false, true) => 'B',
                    (false, false) => ' ',
                }
            })
            .collect();
        let _ = writeln!(out, "{}", row.trim_end());
    }
    let colors: BTreeSet<String> = buf
        .content
        .iter()
        .flat_map(|cell| [color_name(cell.fg), color_name(cell.bg)])
        .collect();
    let _ = writeln!(
        out,
        "--- colours ---\n{}",
        colors.into_iter().collect::<Vec<_>>().join(" ")
    );
    out
}

/// Runs only inside a child: draw the frame and write it where asked.
#[test]
fn child_draws_its_frame() {
    if let Some(out) = std::env::var_os(CHILD) {
        std::fs::write(out, dump(&frame())).unwrap();
    }
}

/// The frame as drawn with `ui.theme = theme` and the given environment.
fn themed(name: &str, theme: &str, no_color: bool) -> String {
    let dir = std::env::temp_dir().join(format!("rrover-theme-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json5"),
        format!(r#"{{ "ui": {{ "theme": "{theme}" }} }}"#),
    )
    .unwrap();
    let out = dir.join("frame.txt");

    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--exact", "child_draws_its_frame", "--test-threads=1"])
        .env(CHILD, &out)
        .env("REDIS_ROVER_CONFIG", &dir)
        .env("RROVER_CONFIG", &dir)
        .env("COLORTERM", "truecolor")
        .env("TZ", "UTC")
        .env_remove("NO_COLOR");
    if no_color {
        child.env("NO_COLOR", "1");
    }
    let status = child.output().unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stdout)
    );
    let text = std::fs::read_to_string(&out).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    text
}

fn check(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing {}; run `UPDATE_GOLDEN=1 cargo test --test golden_themes`",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "golden frame `{name}` changed:\n{actual}\nIf intended: UPDATE_GOLDEN=1 cargo test --test golden_themes"
    );
}

#[test]
fn theme_dark() {
    check("theme_dark", &themed("dark", "dark", false));
}

#[test]
fn theme_light() {
    check("theme_light", &themed("light", "light", false));
}

#[test]
fn theme_high_contrast() {
    check(
        "theme_high_contrast",
        &themed("high-contrast", "high-contrast", false),
    );
}

#[test]
fn theme_monochrome() {
    check("theme_monochrome", &themed("mono", "monochrome", false));
}

/// AC2: with `NO_COLOR` no cell has a colour, and every state still shows.
#[test]
fn no_color_keeps_every_state_visible() {
    let frame = themed("no-color", "dark", true);
    check("theme_no_color", &frame);

    let colours = frame.rsplit("--- colours ---\n").next().unwrap();
    assert_eq!(colours.trim(), "Reset", "no colour at all");
    let (text, attributes) = frame.split_once("--- attributes").unwrap();
    assert!(text.contains("PROD · RO"), "environment");
    assert!(text.contains("● live"), "liveness");
    let rows: Vec<&str> = text.lines().collect();
    let selected = rows.iter().position(|row| row.contains('▌')).unwrap();
    assert!(rows[selected].contains('●'), "mark");
    let attribute_rows: Vec<&str> = attributes.lines().skip(1).collect();
    assert!(
        attribute_rows[selected].contains('#') || attribute_rows[selected].contains('R'),
        "selection is reverse video"
    );
}
