use std::borrow::Cow;

use byte_unit::{Byte, UnitType};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    redis_client::types::{
        JsonPreview, JsonPreviewKind, KeyValue, LoadedValue, ValuePageInfo, STRING_PREVIEW_BYTES,
    },
    utils::{format_duration, group_thousands},
};

/// Redis `TTL` sentinel for a key that exists but has no expiry.
pub(super) const TTL_NO_EXPIRY: isize = -1;

/// Compact TTL for the table column: `∞` for no expiry, `—` for a missing key,
/// otherwise the two most significant units (`45s`, `1h 30m`, `2d 4h`).
pub(super) fn format_ttl(ttl: isize) -> String {
    match ttl {
        TTL_NO_EXPIRY => "∞".into(),
        t if t < 0 => "—".into(),
        t => format_duration(t.unsigned_abs()),
    }
}

/// Detail-pane TTL: spelled-out sentinels, and the raw seconds alongside the
/// humanized duration (`1h 30m (5400s)`).
pub(super) fn format_ttl_detail(ttl: isize) -> String {
    match ttl {
        TTL_NO_EXPIRY => "No expiry".into(),
        t if t < 0 => "Missing".into(),
        t => format!("{} ({t}s)", format_duration(t.unsigned_abs())),
    }
}

/// One-line paging / truncation summary shown under the value, e.g.
/// `Items 101–200 of 12,345 · n/p page`. `None` when there's nothing to say
/// (an untruncated STRING, an empty collection, an unsupported type).
/// `json_path` is the JSON field opened from a preview, if any.
pub(super) fn value_footer(
    loaded: &LoadedValue,
    loading: bool,
    json_path: &[String],
) -> Option<String> {
    let noun = match &loaded.value {
        KeyValue::String { value, total_len } => {
            if !loaded.value.is_truncated() {
                return None;
            }
            let shown = format_bytes(value.len().min(STRING_PREVIEW_BYTES));
            let total = format_bytes(*total_len);
            return Some(format!("Showing first {shown} of {total}"));
        }
        KeyValue::List(_) => "Items",
        KeyValue::Hash(_) => "Fields",
        KeyValue::Set(_) | KeyValue::Zset(_) => "Members",
        KeyValue::Json(_) => {
            return (!json_path.is_empty())
                .then(|| format!("{} · Esc back", display_json_path(json_path)));
        }
        KeyValue::JsonPreview(preview) => {
            return Some(json_preview_footer(
                preview,
                loaded.page,
                loading,
                json_path,
            ));
        }
        KeyValue::Unknown => return None,
    };
    let page = loaded.page;
    if page.len == 0 {
        return None;
    }
    let mut footer = format!(
        "{noun} {}–{}",
        group_thousands(page.start + 1),
        group_thousands(page.start + page.len)
    );
    if let Some(total) = page.total {
        footer.push_str(" of ");
        footer.push_str(&group_thousands(total));
    }
    if page.is_paged() {
        footer.push_str(" · n/p page");
    }
    if loading {
        footer.push_str(" · loading…");
    }
    Some(footer)
}

/// `14.21 MiB preview · fields 1–100 of 50,000 · n/p page · Enter opens`, led
/// by the opened field's path, if any. Kept short: it must fit the value pane
/// at 80 columns.
fn json_preview_footer(
    preview: &JsonPreview,
    page: ValuePageInfo,
    loading: bool,
    json_path: &[String],
) -> String {
    let mut parts = Vec::new();
    if !json_path.is_empty() {
        parts.push(display_json_path(json_path));
    }
    parts.push(format!(
        "{} preview",
        format_bytes_u128(u128::from(preview.bytes))
    ));
    match &preview.kind {
        JsonPreviewKind::Object { len, fields } => {
            if !fields.is_empty() {
                parts.push(format!(
                    "fields {}–{} of {}",
                    group_thousands(page.start + 1),
                    group_thousands(page.start + fields.len()),
                    group_thousands(*len)
                ));
            }
            if page.is_paged() {
                parts.push("n/p page".into());
            }
            parts.push("Enter opens".into());
        }
        JsonPreviewKind::Array { len, items } => {
            let shown = items.as_array().map_or(0, Vec::len);
            parts.push(format!(
                "first {} of {} items",
                group_thousands(shown),
                group_thousands(*len)
            ));
        }
        JsonPreviewKind::TooLarge { .. } => {}
    }
    if !json_path.is_empty() {
        parts.push("Esc back".into());
    }
    if loading {
        parts.push("loading…".into());
    }
    parts.join(" · ")
}

/// A JSON field path as shown to the user: `$.orders["2024-01"]`. Plain
/// identifiers use dot notation, anything else a quoted bracket.
pub(super) fn display_json_path(path: &[String]) -> String {
    let mut out = String::from("$");
    for name in path {
        let plain = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if plain {
            out.push('.');
            out.push_str(name);
        } else {
            out.push('[');
            out.push_str(&serde_json::Value::from(name.as_str()).to_string());
            out.push(']');
        }
    }
    out
}

/// Human byte size, matching the Size field (`64.00 KiB`).
pub fn format_bytes(n: usize) -> String {
    format_bytes_u128(u128::try_from(n).unwrap_or(u128::MAX))
}

pub(super) fn format_bytes_u128(n: u128) -> String {
    let byte = Byte::from_u128(n)
        .unwrap_or_default()
        .get_appropriate_unit(UnitType::Binary);
    format!("{byte:.2}")
}

/// Cuts `s` to `max` display columns, ending in `…` when it was longer.
pub(crate) fn truncate_to_width(s: &str, max: usize) -> Cow<'_, str> {
    if s.width() <= max {
        return Cow::Borrowed(s);
    }
    let Some(budget) = max.checked_sub(1) else {
        return Cow::Borrowed("");
    };
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_client::types::{KeyValue, LoadedValue, ValuePageInfo};
    use crate::utils::group_thousands;
    use crate::widgets::keyspace::tests::list_page;

    fn object_preview(fields: usize, len: usize) -> LoadedValue {
        LoadedValue {
            value: KeyValue::JsonPreview(JsonPreview {
                bytes: 14_900_000,
                kind: JsonPreviewKind::Object {
                    len,
                    fields: (0..fields)
                        .map(|i| crate::redis_client::types::JsonField {
                            name: format!("f{i}"),
                            json_type: "object".into(),
                            bytes: 10,
                        })
                        .collect(),
                },
            }),
            page: ValuePageInfo {
                start: 0,
                len: fields,
                total: Some(len),
                has_next: fields < len,
                has_prev: false,
            },
        }
    }

    #[test]
    fn json_preview_footer_text() {
        assert_eq!(
            value_footer(&object_preview(100, 50_000), false, &[]).as_deref(),
            Some("14.21 MiB preview · fields 1–100 of 50,000 · n/p page · Enter opens")
        );
        let opened = ["orders".to_string(), "2024-01".to_string()];
        assert_eq!(
            value_footer(&object_preview(2, 2), true, &opened).as_deref(),
            Some(
                "$.orders[\"2024-01\"] · 14.21 MiB preview · fields 1–2 of 2 · Enter opens · \
                 Esc back · loading…"
            )
        );

        let array = LoadedValue {
            value: KeyValue::JsonPreview(JsonPreview {
                bytes: 2 << 20,
                kind: JsonPreviewKind::Array {
                    len: 1_000_000,
                    items: serde_json::json!([1, 2, 3]),
                },
            }),
            page: ValuePageInfo::default(),
        };
        assert_eq!(
            value_footer(&array, false, &[]).as_deref(),
            Some("2.00 MiB preview · first 3 of 1,000,000 items")
        );

        let field = LoadedValue {
            value: KeyValue::Json(serde_json::json!({"a": 1})),
            page: ValuePageInfo::default(),
        };
        assert_eq!(value_footer(&field, false, &[]), None, "a whole document");
        assert_eq!(
            value_footer(&field, false, &["orders".into()]).as_deref(),
            Some("$.orders · Esc back")
        );
    }

    #[test]
    fn value_footer_text() {
        let page = list_page(100, 100, true, true);
        assert_eq!(
            value_footer(&page, false, &[]).as_deref(),
            Some("Items 101–200 of 1,000 · n/p page")
        );
        assert_eq!(
            value_footer(&page, true, &[]).as_deref(),
            Some("Items 101–200 of 1,000 · n/p page · loading…")
        );

        let small = LoadedValue {
            value: KeyValue::Set(vec!["a".into()]),
            page: ValuePageInfo {
                len: 1,
                total: Some(1),
                ..ValuePageInfo::default()
            },
        };
        assert_eq!(
            value_footer(&small, false, &[]).as_deref(),
            Some("Members 1–1 of 1")
        );

        let full = LoadedValue {
            value: KeyValue::String {
                value: "abc".into(),
                total_len: 3,
            },
            page: ValuePageInfo::default(),
        };
        assert_eq!(value_footer(&full, false, &[]), None);

        let cut = LoadedValue {
            value: KeyValue::String {
                value: "x".repeat(1024),
                total_len: 2048,
            },
            page: ValuePageInfo::default(),
        };
        assert_eq!(
            value_footer(&cut, false, &[]).as_deref(),
            Some("Showing first 1.00 KiB of 2.00 KiB")
        );
    }

    #[test]
    fn thousands_grouping() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn ttl_sentinels() {
        assert_eq!(format_ttl(-1), "∞");
        assert_eq!(format_ttl(-2), "—");
        assert_eq!(format_ttl_detail(-1), "No expiry");
        assert_eq!(format_ttl_detail(-2), "Missing");
    }

    #[test]
    fn ttl_durations() {
        assert_eq!(format_ttl(0), "0s");
        assert_eq!(format_ttl(45), "45s");
        assert_eq!(format_ttl(60), "1m");
        assert_eq!(format_ttl(3600), "1h");
        assert_eq!(format_ttl(5400), "1h 30m");
        assert_eq!(format_ttl(3601), "1h");
        assert_eq!(format_ttl(90_061), "1d 1h");
    }

    #[test]
    fn ttl_detail_includes_raw_seconds() {
        assert_eq!(format_ttl_detail(5400), "1h 30m (5400s)");
        assert_eq!(format_ttl_detail(45), "45s (45s)");
    }

    #[test]
    fn truncate_to_width_marks_cut_names() {
        use super::truncate_to_width;
        use std::borrow::Cow;
        use unicode_width::UnicodeWidthStr;

        assert!(matches!(
            truncate_to_width("short", 10),
            Cow::Borrowed("short")
        ));
        assert_eq!(truncate_to_width("exact", 5), "exact");
        assert_eq!(
            truncate_to_width("app:config:feature_flags", 18),
            "app:config:featur…"
        );
        // Wide chars: count columns, not bytes or chars.
        let cut = truncate_to_width("キーキーキー", 6);
        assert_eq!(cut, "キー…");
        assert!(cut.width() <= 6);
        assert_eq!(truncate_to_width("キーキー", 4), "キ…");
        assert_eq!(truncate_to_width("abc", 1), "…");
        assert_eq!(truncate_to_width("abc", 0), "");
    }
}
