//! Render benches on `ratatui`'s `TestBackend`: the key table, and a 5 MB text
//! value through `TextCache` + `TextView`. Budget: 16 ms per frame.

mod support;

use std::{fmt::Write as _, hint::black_box};

use criterion::Criterion;
use ratatui::{backend::TestBackend, Terminal};
use redis_rover::internals::{
    render_text, KeyMeta, KeySpace, KeySpaceWidget, KeyValue, RedisType, TextCache, TextLabels,
    TextView,
};

#[global_allocator]
static ALLOC: support::Counting = support::Counting;

const WIDTH: u16 = 120;
const HEIGHT: u16 = 40;
/// A text value of about this many bytes.
const TEXT_BYTES: usize = 5 * 1024 * 1024;

fn terminal() -> Terminal<TestBackend> {
    match Terminal::new(TestBackend::new(WIDTH, HEIGHT)) {
        Ok(terminal) => terminal,
        Err(err) => panic!("TestBackend terminal: {err}"),
    }
}

/// 100 keys of every type, with and without TTL.
fn key_rows() -> Vec<KeyMeta> {
    let types = [
        RedisType::String,
        RedisType::Hash,
        RedisType::List,
        RedisType::Set,
        RedisType::Zset,
        RedisType::Json,
        RedisType::Stream,
    ];
    (0..100)
        .map(|i| KeyMeta {
            key: format!("user:{i}:profile:settings"),
            r_type: types[i % types.len()].clone(),
            size: 48 + 1024 * (i as u128),
            ttl: if i % 3 == 0 { -1 } else { 3600 },
            value: KeyValue::Unknown,
        })
        .collect()
}

/// About 5 MB of log-like lines.
fn plain_text() -> String {
    let mut text = String::with_capacity(TEXT_BYTES + 128);
    let mut i = 0;
    while text.len() < TEXT_BYTES {
        let _ = writeln!(
            text,
            "2026-11-07T10:{:02}:{:02}Z INFO request id={i} path=/api/v1/items status=200 took={}ms",
            (i / 60) % 60,
            i % 60,
            i % 250
        );
        i += 1;
    }
    text
}

/// About 5 MB of JSON (an array of objects), pretty-printed by `TextCache`.
fn json_text() -> String {
    let mut text = String::with_capacity(TEXT_BYTES + 128);
    text.push('[');
    let mut i = 0;
    while text.len() < TEXT_BYTES {
        if i > 0 {
            text.push(',');
        }
        let _ = write!(
            text,
            r#"{{"id":{i},"name":"item {i}","tags":["a","b"],"price":{}.5,"active":{}}}"#,
            i % 1000,
            i % 2 == 0
        );
        i += 1;
    }
    text.push(']');
    text
}

fn bench_key_table(c: &mut Criterion) {
    let mut state = KeySpace::new(Vec::new());
    state.set_keys(key_rows());
    let mut terminal = terminal();
    let draw = |terminal: &mut Terminal<TestBackend>, state: &mut KeySpace| {
        if let Err(err) =
            terminal.draw(|frame| frame.render_stateful_widget(KeySpaceWidget, frame.area(), state))
        {
            panic!("draw: {err}");
        }
    };
    // One frame first, so the table has measured its height.
    draw(&mut terminal, &mut state);
    if support::is_bench_run() {
        let ((), mem) = support::measure(|| draw(&mut terminal, &mut state));
        support::record("render/key_table_100", mem, "allocated = one frame");
    }
    c.bench_function("render/key_table_100", |b| {
        b.iter(|| draw(&mut terminal, &mut state));
    });
}

fn bench_text(c: &mut Criterion) {
    let plain = plain_text();
    let json = json_text();

    // Building the lines happens once, when the value is loaded.
    if support::is_bench_run() {
        let (cache, mem) = support::measure(|| TextCache::from_text(&plain, false));
        drop(cache);
        support::record("text_cache/5MB_plain", mem, "retained = the cached lines");
        let (cache, mem) = support::measure(|| TextCache::from_text(&json, true));
        drop(cache);
        support::record("text_cache/5MB_json", mem, "retained = the cached lines");
    }
    let mut group = c.benchmark_group("text_cache");
    group.sample_size(10);
    group.bench_function("5MB_plain", |b| {
        b.iter(|| TextCache::from_text(black_box(&plain), false));
    });
    group.bench_function("5MB_json", |b| {
        b.iter(|| TextCache::from_text(black_box(&json), true));
    });
    group.finish();

    // Drawing happens every frame while the value is on screen.
    let labels = TextLabels {
        header: "Value",
        paging: false,
        close: None,
    };
    let mut group = c.benchmark_group("render_text");
    group.sample_size(10);
    for (id, text, try_json, wrap) in [
        ("5MB_plain_wrap", &plain, false, true),
        ("5MB_plain_nowrap", &plain, false, false),
        ("5MB_json_wrap", &json, true, true),
    ] {
        let cache = TextCache::from_text(text, try_json);
        let mut view = TextView::default();
        if !wrap {
            view.toggle_wrap();
        }
        let mut terminal = terminal();
        let draw = |terminal: &mut Terminal<TestBackend>, view: &mut TextView| {
            if let Err(err) = terminal.draw(|frame| {
                let area = frame.area();
                render_text(&cache, &labels, None, true, area, frame.buffer_mut(), view);
            }) {
                panic!("draw: {err}");
            }
        };
        draw(&mut terminal, &mut view);
        if support::is_bench_run() {
            let ((), mem) = support::measure(|| draw(&mut terminal, &mut view));
            support::record(&format!("render_text/{id}"), mem, "allocated = one frame");
        }
        group.bench_function(id, |b| b.iter(|| draw(&mut terminal, &mut view)));
    }
    group.finish();
}

fn main() {
    support::use_default_config();
    let mut c = Criterion::default().configure_from_args();
    bench_key_table(&mut c);
    bench_text(&mut c);
    c.final_summary();
}
