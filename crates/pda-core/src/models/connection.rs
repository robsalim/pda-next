//! Подключение к устройству.

use serde::{Deserialize, Serialize};

use super::ConnConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: i64,
    pub name: String,
    /// Канонические имена: 'snap7', 'snap7_s200', 'modbus_client', 'modbus_server'
    pub driver_type: String,
    pub enabled: bool,
    pub poll_interval_ms: i64,
    pub config: ConnConfig,
}

impl Connection {
    /// Приводит старое написание типа к каноническому.
    pub fn new(
        id: i64,
        name: impl Into<String>,
        driver_type: impl Into<String>,
        enabled: bool,
        poll_interval_ms: i64,
        config: ConnConfig,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            driver_type: crate::drivers::registry::normalize_driver_type(&driver_type.into()),
            enabled,
            poll_interval_ms,
            config,
        }
    }
}
