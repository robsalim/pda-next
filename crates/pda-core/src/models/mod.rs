//! Модели данных.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub mod tag;
pub mod connection;
pub mod data_point;

pub use tag::Tag;
pub use connection::Connection;
pub use data_point::DataPoint;

/// Конфигурация подключения — map произвольных JSON-значений.
pub type ConnConfig = HashMap<String, serde_json::Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Engine {
    #[serde(rename = "sqlite")]
    Sqlite,
    #[serde(rename = "postgres")]
    Postgres,
    #[serde(rename = "mysql")]
    Mysql,
    #[serde(rename = "mariadb")]
    Mariadb,
}

/// Значение поля config как строка (для сообщений и параметров драйверов).
pub fn cfg_str(cfg: &ConnConfig, key: &str, default: &str) -> String {
    match cfg.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        _ => default.to_string(),
    }
}

pub fn cfg_i64(cfg: &ConnConfig, key: &str, default: i64) -> i64 {
    match cfg.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(default),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(default),
        _ => default,
    }
}

pub fn cfg_f64(cfg: &ConnConfig, key: &str, default: f64) -> f64 {
    match cfg.get(key) {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(default),
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(default),
        _ => default,
    }
}
