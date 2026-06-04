use super::types::RedisType;

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
}
