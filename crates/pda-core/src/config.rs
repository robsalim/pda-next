//! Глобальные настройки приложения и журнала.

use std::collections::HashMap;

/// Значения по умолчанию для DB_CONFIG (PostgreSQL).
pub fn db_config_default() -> HashMap<String, serde_json::Value> {
    let mut m = HashMap::new();
    m.insert("dbname".into(), serde_json::json!("pda_data"));
    m.insert("user".into(), serde_json::json!("postgres"));
    m.insert("password".into(), serde_json::json!("25052015"));
    m.insert("host".into(), serde_json::json!("localhost"));
    m.insert("port".into(), serde_json::json!(5432));
    m.insert("client_encoding".into(), serde_json::json!("utf-8"));
    m
}

pub const DEFAULT_MODBUS_SERVER_PORT: u16 = 502;
pub const WEB_HOST: &str = "0.0.0.0";
pub const WEB_PORT: u16 = 8082;
pub const APP_NAME: &str = "PDA Next";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Настройки журнала. Все значения в миллисекундах.
#[derive(Debug, Clone)]
pub struct JournalConfig {
    /// как часто выбирать подлежащие записи точки (journal_scan_ms)
    pub journal_scan_ms: u64,
    /// как часто (минимум) сливать очередь в БД (journal_flush_ms)
    pub journal_flush_ms: u64,
    /// максимум точек в одном INSERT (journal_batch)
    pub journal_batch: usize,
    /// глубина буфера; при переполнении точки теряются (journal_queue)
    pub journal_queue: usize,
    /// 0 = не чистить историю; N = хранить N дней (retention_days)
    pub retention_days: f64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            journal_scan_ms: 20,
            journal_flush_ms: 50,
            journal_batch: 2000,
            journal_queue: 200_000,
            retention_days: 0.0,
        }
    }
}
