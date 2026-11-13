//! Narrowing the key list to some Redis types.
//!
//! With exactly one type the sweep asks the server (`SCAN … TYPE t`, Redis
//! 6+). Otherwise, or when the server rejects `TYPE`, the runner asks `TYPE`
//! for each SCAN batch and keeps the matching keys. Either way only matching
//! keys enter the sweep, so paging, sorting and counts work unchanged.

use super::RedisType;

/// The types the user picked. Empty means every type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TypeFilter {
    /// Type names as [`RedisType::name`] gives them (`json`, not
    /// `ReJSON-RL`), sorted and unique.
    names: Vec<String>,
}

/// The types the checklist always offers, in the order shown.
pub const STANDARD_TYPES: [&str; 7] = ["string", "list", "set", "zset", "hash", "stream", "json"];

impl TypeFilter {
    #[must_use]
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut names: Vec<String> = names
            .into_iter()
            .map(|name| RedisType::from(name.as_ref()).name().to_owned())
            .collect();
        names.sort_unstable();
        names.dedup();
        Self { names }
    }

    /// No type picked: every key is shown.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    #[must_use]
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// A `TYPE` reply (`ReJSON-RL` included) is one of the picked types.
    #[must_use]
    pub fn matches(&self, type_reply: &str) -> bool {
        self.is_empty() || {
            let r_type = RedisType::from(type_reply);
            self.names.iter().any(|name| name == r_type.name())
        }
    }

    /// The `TYPE` argument of a server-side `SCAN`, when exactly one type is
    /// picked. `RedisJSON` keys go by their module type name.
    #[must_use]
    pub fn scan_type(&self) -> Option<&str> {
        match self.names.as_slice() {
            [name] if name == "json" => Some("ReJSON-RL"),
            [name] => Some(name),
            _ => None,
        }
    }

    /// `hash,zset`, as the filter row shows it.
    #[must_use]
    pub fn label(&self) -> String {
        self.names.join(",")
    }
}

/// How a SCAN step applies the type filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeScan {
    /// No filter.
    All,
    /// `SCAN … TYPE t`.
    Server(String),
    /// Plain SCAN, then `TYPE` per key, keeping the matching ones.
    Client(TypeFilter),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_normalised_sorted_and_unique() {
        let filter = TypeFilter::new(["zset", "hash", "ReJSON-RL", "hash"]);
        assert_eq!(filter.names(), ["hash", "json", "zset"]);
        assert_eq!(filter.label(), "hash,json,zset");
    }

    #[test]
    fn matches_type_replies() {
        let filter = TypeFilter::new(["hash", "json"]);
        assert!(filter.matches("hash"));
        assert!(filter.matches("ReJSON-RL"));
        assert!(!filter.matches("string"));
        assert!(TypeFilter::default().matches("string"), "empty = all");
    }

    #[test]
    fn scan_type_only_for_a_single_type() {
        assert_eq!(TypeFilter::new(["hash"]).scan_type(), Some("hash"));
        assert_eq!(TypeFilter::new(["json"]).scan_type(), Some("ReJSON-RL"));
        assert_eq!(TypeFilter::new(["hash", "set"]).scan_type(), None);
        assert_eq!(TypeFilter::default().scan_type(), None);
    }
}
