use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct CommonInfo {
    pub os: String,
    #[serde(rename(deserialize = "redis_version"))]
    pub version: String,
    #[serde(rename(deserialize = "used_memory_human"))]
    pub memory: String,
    #[serde(rename(deserialize = "total_system_memory_human"))]
    pub total_memory: String,
    #[serde(rename(deserialize = "used_cpu_sys"))]
    pub cpu: String,
    #[serde(rename(deserialize = "connected_clients"))]
    pub clients: String,
    #[serde(rename(deserialize = "maxclients"))]
    pub max_clients: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RedisInfo {
    #[serde(flatten)]
    pub common: CommonInfo,
}

impl RedisInfo {
    pub fn redis_version(&self) -> String {
        format!("Redis version: {}", self.common.version)
    }

    pub fn os(&self) -> String {
        format!("OS: {}", self.common.os)
    }

    pub fn cpu(&self) -> String {
        format!("CPU: {}", self.common.cpu)
    }

    pub fn memory(&self) -> String {
        format!("RAM: {}/{}", self.common.memory, self.common.total_memory)
    }

    pub fn clients(&self) -> String {
        format!(
            "Connected clients: {}/{}",
            self.common.clients, self.common.max_clients
        )
    }
}

pub struct KeysPagination {
    pub cursor: Option<String>,
    pub pattern: Option<String>,
    pub count: usize,
}
