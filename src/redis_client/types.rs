use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct CommonInfo {
    #[serde(rename(deserialize = "redis_version"))]
    pub version: String,
    #[serde(rename(deserialize = "used_memory_human"))]
    pub memory: String,
    #[serde(rename(deserialize = "total_system_memory_human"))]
    pub total_memory: String,
    pub os: String,
}

#[derive(Debug, Deserialize)]
pub struct RedisInfo {
    #[serde(flatten)]
    pub common: CommonInfo,
}
