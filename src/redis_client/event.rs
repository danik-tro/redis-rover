#[derive(Clone, Debug)]
pub enum RedisEvent {
    FetchKeys {
        cursor: Option<usize>,
        pattern: Option<String>,
    },
}
