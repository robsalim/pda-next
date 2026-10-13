//! Тег конфигурации.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: i64,
    pub connection_id: i64,
    pub name: String,
    /// Например "0", "100", "DB1.DBD0", "DB1.DBX0.0"
    pub address_str: String,
    /// 'FLOAT', 'INT16', 'UINT16', 'BOOL'
    pub data_type: String,
    pub scale: f64,
    pub offset_val: f64,
    pub unit: String,
    pub group_name: String,
    /// Собственная частота регистрации в историю, мс (0 = только по изменению).
    pub log_interval_ms: i64,
    /// Мёртвая зона: изменение меньше порога в историю не пишется.
    pub deadband: f64,
}

impl Tag {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: i64,
        connection_id: i64,
        name: impl Into<String>,
        address_str: impl Into<String>,
        data_type: impl Into<String>,
        scale: f64,
        offset_val: f64,
        unit: impl Into<String>,
        group_name: impl Into<String>,
        log_interval_ms: i64,
        deadband: f64,
    ) -> Self {
        Self {
            id,
            connection_id,
            name: name.into(),
            address_str: address_str.into(),
            data_type: data_type.into(),
            scale,
            offset_val,
            unit: unit.into(),
            group_name: group_name.into(),
            log_interval_ms,
            deadband,
        }
    }
}

impl Default for Tag {
    fn default() -> Self {
        Self {
            id: 0,
            connection_id: 0,
            name: String::new(),
            address_str: String::new(),
            data_type: "FLOAT".into(),
            scale: 1.0,
            offset_val: 0.0,
            unit: String::new(),
            group_name: "Общие".into(),
            log_interval_ms: 1000,
            deadband: 0.0,
        }
    }
}
