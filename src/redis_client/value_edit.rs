//! A whole collection edited as JSON in `$EDITOR` (`Shift-e` on a hash,
//! list, set or zset key), turned back into the writes that make Redis
//! match it.
//!
//! The document is what `y` copies ([`KeyValue::to_clipboard_text`]): a
//! hash is an object of strings, a list or set an array of strings, a zset
//! an array of `{"member", "score"}` (`"inf"` / `"-inf"` for infinite
//! scores). [`diff`] reads the value as fetched and as saved and returns
//! only what changed, so a hash / set / zset edit leaves the elements it
//! didn't touch (and changes other clients made to them) alone. A list has
//! no stable identity per element, so a changed list is replaced whole.
//!
//! [`KeyValue::to_clipboard_text`]: super::types::KeyValue::to_clipboard_text

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::types::RedisType;

/// The writes that turn the fetched value into the edited one.
#[derive(Debug, Clone, PartialEq)]
pub enum ValueEdit {
    /// `HSET` the new / changed fields, `HDEL` the removed ones.
    Hash {
        set: Vec<(String, String)>,
        del: Vec<String>,
    },
    /// `SADD` / `SREM`.
    Set {
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// `ZADD` new members and new scores, `ZREM` the removed ones.
    Zset {
        add: Vec<(String, f64)>,
        remove: Vec<String>,
    },
    /// `DEL` + `RPUSH` the whole list (its TTL is put back).
    List { items: Vec<String> },
}

impl ValueEdit {
    /// How many elements it writes or removes (for the log and the toast).
    #[must_use]
    pub fn changes(&self) -> usize {
        match self {
            Self::Hash { set, del } => set.len() + del.len(),
            Self::Set { add, remove } => add.len() + remove.len(),
            Self::Zset { add, remove } => add.len() + remove.len(),
            Self::List { items } => items.len(),
        }
    }

    /// It removes elements (or replaces the list).
    #[must_use]
    pub fn removes(&self) -> bool {
        match self {
            Self::Hash { del, .. } => !del.is_empty(),
            Self::Set { remove, .. } | Self::Zset { remove, .. } => !remove.is_empty(),
            Self::List { .. } => true,
        }
    }
}

/// What [`diff`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Diff {
    /// Nothing changed.
    Same,
    /// These writes; `empties` when the edited collection is empty, which
    /// deletes the key.
    Changed { edit: ValueEdit, empties: bool },
}

/// The edit that turns `old` (the fetched document) into `new` (the saved
/// one) for a key of type `r_type`.
///
/// # Errors
///
/// A message for the user when `new` isn't valid JSON or not the shape
/// the type needs (an object of strings for a hash, …).
pub fn diff(r_type: &RedisType, old: &str, new: &str) -> Result<Diff, String> {
    let parse = |text: &str| -> Result<Value, String> {
        serde_json::from_str(text).map_err(|err| format!("not valid JSON: {err}"))
    };
    let (old, new) = (parse(old)?, parse(new)?);
    match r_type {
        RedisType::Hash => {
            let old = hash_fields(&old)?;
            let new = hash_fields(&new)?;
            let before: HashMap<&str, &str> =
                old.iter().map(|(f, v)| (f.as_str(), v.as_str())).collect();
            let set: Vec<(String, String)> = new
                .iter()
                .filter(|(f, v)| before.get(f.as_str()) != Some(&v.as_str()))
                .cloned()
                .collect();
            let kept: HashSet<&str> = new.iter().map(|(f, _)| f.as_str()).collect();
            let del: Vec<String> = old
                .iter()
                .filter(|(f, _)| !kept.contains(f.as_str()))
                .map(|(f, _)| f.clone())
                .collect();
            Ok(changed(
                set.is_empty() && del.is_empty(),
                new.is_empty(),
                ValueEdit::Hash { set, del },
            ))
        }
        RedisType::Set => {
            let old = strings(&old, "a set")?;
            let new = strings(&new, "a set")?;
            let before: HashSet<&str> = old.iter().map(String::as_str).collect();
            let after: HashSet<&str> = new.iter().map(String::as_str).collect();
            let mut add: Vec<String> = Vec::new();
            for member in &new {
                if !before.contains(member.as_str()) && !add.contains(member) {
                    add.push(member.clone());
                }
            }
            let remove: Vec<String> = old
                .iter()
                .filter(|m| !after.contains(m.as_str()))
                .cloned()
                .collect();
            Ok(changed(
                add.is_empty() && remove.is_empty(),
                after.is_empty(),
                ValueEdit::Set { add, remove },
            ))
        }
        RedisType::Zset => {
            let old = zset_members(&old)?;
            let new = zset_members(&new)?;
            let before: HashMap<&str, f64> = old.iter().map(|(m, s)| (m.as_str(), *s)).collect();
            #[allow(clippy::float_cmp)] // a score typed back unchanged is exactly equal
            let add: Vec<(String, f64)> = new
                .iter()
                .filter(|(m, s)| before.get(m.as_str()) != Some(s))
                .cloned()
                .collect();
            let kept: HashSet<&str> = new.iter().map(|(m, _)| m.as_str()).collect();
            let remove: Vec<String> = old
                .iter()
                .filter(|(m, _)| !kept.contains(m.as_str()))
                .map(|(m, _)| m.clone())
                .collect();
            Ok(changed(
                add.is_empty() && remove.is_empty(),
                new.is_empty(),
                ValueEdit::Zset { add, remove },
            ))
        }
        RedisType::List => {
            let old = strings(&old, "a list")?;
            let new = strings(&new, "a list")?;
            Ok(changed(
                old == new,
                new.is_empty(),
                ValueEdit::List { items: new },
            ))
        }
        other => Err(format!("{} values can't be edited this way", other.name())),
    }
}

fn changed(same: bool, empties: bool, edit: ValueEdit) -> Diff {
    if same {
        Diff::Same
    } else {
        Diff::Changed { edit, empties }
    }
}

/// An element as Redis stores it: a JSON string as is; a number or a
/// boolean as its text (`42`, `true`), since Redis keeps only strings.
fn element(value: &Value, what: &str) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => Err(format!(
            "{what} must be a string, not {}",
            match value {
                Value::Null => "null",
                Value::Array(_) => "an array",
                _ => "an object",
            }
        )),
    }
}

/// A hash document: `{"field": "value", …}`. A field written twice keeps
/// its last value, as JSON parsers do.
fn hash_fields(doc: &Value) -> Result<Vec<(String, String)>, String> {
    let Value::Object(map) = doc else {
        return Err(r#"a hash is an object: {"field": "value", …}"#.to_owned());
    };
    map.iter()
        .map(|(field, value)| Ok((field.clone(), element(value, &format!("field {field:?}"))?)))
        .collect()
}

/// A list or set document: `["a", "b", …]`.
fn strings(doc: &Value, what: &str) -> Result<Vec<String>, String> {
    let Value::Array(items) = doc else {
        return Err(format!(r#"{what} is an array: ["a", "b", …]"#));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| element(item, &format!("element {i}")))
        .collect()
}

/// A zset document: `[{"member": "a", "score": 1}, …]`; a member written
/// twice keeps its last score.
fn zset_members(doc: &Value) -> Result<Vec<(String, f64)>, String> {
    const SHAPE: &str = r#"a zset is an array of {"member": "a", "score": 1}"#;
    let Value::Array(items) = doc else {
        return Err(SHAPE.to_owned());
    };
    let mut members: Vec<(String, f64)> = Vec::with_capacity(items.len());
    let mut at: HashMap<String, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        let (Some(member), Some(score)) = (item.get("member"), item.get("score")) else {
            return Err(format!("element {i}: {SHAPE}"));
        };
        let member = element(member, &format!("element {i}'s member"))?;
        let score = score_of(score)
            .ok_or_else(|| format!("element {i}'s score must be a number, \"inf\" or \"-inf\""))?;
        if let Some(&j) = at.get(&member) {
            members[j].1 = score;
        } else {
            at.insert(member.clone(), members.len());
            members.push((member, score));
        }
    }
    Ok(members)
}

/// A score: a JSON number, or `"inf"` / `"+inf"` / `"-inf"` / a number as
/// a string.
#[must_use]
pub fn score_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_score(s),
        _ => None,
    }
}

/// A score typed as text: a number, `inf`, `+inf` or `-inf`. NaN is not a
/// score.
#[must_use]
pub fn parse_score(text: &str) -> Option<f64> {
    let text = text.trim();
    let score = match text.to_ascii_lowercase().as_str() {
        "inf" | "+inf" => f64::INFINITY,
        "-inf" => f64::NEG_INFINITY,
        _ => text.parse::<f64>().ok()?,
    };
    (!score.is_nan()).then_some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(r_type: &RedisType, old: &str, new: &str) -> ValueEdit {
        match diff(r_type, old, new) {
            Ok(Diff::Changed { edit, .. }) => edit,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hash_writes_only_what_changed() {
        let old = r#"{"name": "Ada", "email": "a@x", "age": "36"}"#;
        let new = r#"{"name": "Ada L", "age": 36, "city": "London"}"#;
        assert_eq!(
            edit(&RedisType::Hash, old, new),
            ValueEdit::Hash {
                set: vec![
                    ("name".into(), "Ada L".into()),
                    ("city".into(), "London".into())
                ],
                del: vec!["email".into()],
            },
            "36 as a number is the same \"36\""
        );
        assert_eq!(diff(&RedisType::Hash, old, old), Ok(Diff::Same));
        assert!(matches!(
            diff(&RedisType::Hash, old, "{}"),
            Ok(Diff::Changed { empties: true, .. })
        ));
    }

    #[test]
    fn a_set_adds_and_removes_members() {
        assert_eq!(
            edit(
                &RedisType::Set,
                r#"["a", "b", "c"]"#,
                r#"["c", "d", "a", "d"]"#
            ),
            ValueEdit::Set {
                add: vec!["d".into()],
                remove: vec!["b".into()],
            }
        );
        assert_eq!(
            diff(&RedisType::Set, r#"["a","b"]"#, r#"["b","a"]"#),
            Ok(Diff::Same),
            "order doesn't matter in a set"
        );
    }

    #[test]
    fn a_zset_changes_scores_and_members() {
        let old = r#"[{"member": "ada", "score": 42}, {"member": "bob", "score": "inf"}]"#;
        let new = r#"[{"member": "ada", "score": 43.5}, {"member": "bob", "score": "+inf"},
                      {"member": "eve", "score": "-inf"}]"#;
        assert_eq!(
            edit(&RedisType::Zset, old, new),
            ValueEdit::Zset {
                add: vec![("ada".into(), 43.5), ("eve".into(), f64::NEG_INFINITY)],
                remove: vec![],
            }
        );
        let err = diff(&RedisType::Zset, old, r#"[{"member": "a", "score": "x"}]"#).unwrap_err();
        assert!(err.contains("score"), "{err}");
    }

    #[test]
    fn a_changed_list_is_replaced_whole() {
        assert_eq!(
            edit(&RedisType::List, r#"["a", "b"]"#, r#"["b", "a", "a"]"#),
            ValueEdit::List {
                items: vec!["b".into(), "a".into(), "a".into()],
            }
        );
        assert_eq!(
            diff(&RedisType::List, r#"["a"]"#, r#"["a"]"#),
            Ok(Diff::Same)
        );
        assert!(matches!(
            diff(&RedisType::List, r#"["a"]"#, "[]"),
            Ok(Diff::Changed { empties: true, .. })
        ));
    }

    #[test]
    fn bad_documents_say_what_is_expected() {
        for (r_type, new, needle) in [
            (RedisType::Hash, "[1]", "object"),
            (RedisType::Hash, r#"{"a": null}"#, "string"),
            (RedisType::List, r#"{"a": 1}"#, "array"),
            (RedisType::Set, r#"[["x"]]"#, "string"),
            (RedisType::Zset, r#"[{"member": "a"}]"#, "member"),
            (RedisType::List, "[\"a\",", "valid JSON"),
        ] {
            let old = if r_type == RedisType::Hash {
                "{}"
            } else {
                "[]"
            };
            let err = diff(&r_type, old, new).unwrap_err();
            assert!(err.contains(needle), "{r_type:?} {new}: {err}");
        }
    }

    #[test]
    fn scores_parse_like_redis() {
        assert_eq!(parse_score(" 42 "), Some(42.0));
        assert_eq!(parse_score("-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_score("+INF"), Some(f64::INFINITY));
        assert_eq!(parse_score("nan"), None);
        assert_eq!(parse_score("x"), None);
    }
}
