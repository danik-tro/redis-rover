use super::types::{KeyItem, NewKeySpec, RedisType};

#[derive(Clone, Debug)]
pub enum RedisEvent {
    FetchKeys,
    FetchValue {
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
}
