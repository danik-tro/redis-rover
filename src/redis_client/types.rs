use std::borrow::Cow;
use std::collections::VecDeque;

use crate::config;
use ratatui::{
    style::Stylize,
    text::{Span, Text},
};
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

#[derive(Debug, Clone)]
pub struct KeyMeta {
    pub key: String,
    pub r_type: String,
    pub size: u128,
    pub ttl: isize,
}

pub enum KeysList {
    Empty,
    Keys { cursor: usize, keys: Vec<KeyMeta> },
}

#[derive(Debug)]
pub enum RedisType {
    Set,
    List,
    Hash,
    Zset,
    Json,
    String,
    Unknown,
}

impl RedisType {
    fn from_str<'a>(value: Cow<'a, str>) -> Self {
        match value.as_ref() {
            "string" => Self::String,
            "hash" => Self::Hash,
            "set" => Self::Set,
            "zset" => Self::Zset,
            "json" => Self::Json,
            "list" => Self::List,
            _ => Self::Unknown,
        }
    }
}

impl<'a> Into<Text<'a>> for RedisType {
    fn into(self) -> Text<'a> {
        match self {
            Self::String => Span::raw(" STRING ")
                .bg(config::get().keyspace.string)
                .into(),
            Self::Json => Span::raw(" JSON ").bg(config::get().keyspace.json).into(),
            Self::List => Span::raw(" LIST ").bg(config::get().keyspace.list).into(),
            Self::Set => Span::raw(" SET ").bg(config::get().keyspace.set).into(),
            Self::Zset => Span::raw(" ZSET ").bg(config::get().keyspace.zset).into(),
            Self::Hash => Span::raw(" HASH ").bg(config::get().keyspace.hash).into(),
            Self::Unknown => Span::raw(" ? ").bg(config::get().keyspace.unknown).into(),
        }
    }
}

impl<'a, T> From<T> for RedisType
where
    T: Into<Cow<'a, str>>,
{
    #[inline]
    fn from(value: T) -> Self {
        Self::from_str(value.into())
    }
}

#[derive(Debug, Clone)]
pub struct KeyspaceState {
    pub cursor: Option<usize>,
    pub next_cursor: Option<usize>,
    pub pattern: Option<String>,
    pub count: usize,
    pub cursor_stack: VecDeque<usize>,
}

impl KeyspaceState {
    pub fn set_next_cursor(&mut self, cursor: usize) {
        self.next_cursor = Some(cursor);
    }

    pub fn update_cursor(&mut self) {
        if self.next_cursor == Some(0) {
            return;
        }

        if let Some(cursor) = self.cursor.take() {
            self.cursor_stack.push_back(cursor);
        }
        self.cursor = self.next_cursor.take();
    }

    pub fn set_previous_cursor(&mut self) {
        self.cursor = self.cursor_stack.pop_back();
    }
}

impl Default for KeyspaceState {
    fn default() -> Self {
        Self {
            count: 10,
            pattern: None,
            cursor: None,
            next_cursor: None,
            cursor_stack: VecDeque::new(),
        }
    }
}
