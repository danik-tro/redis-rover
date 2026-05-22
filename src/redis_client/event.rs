use super::types::RedisType;

#[derive(Clone, Debug)]
pub enum RedisEvent {
    FetchKeys,
    FetchValue { key: String, r_type: RedisType },
}
