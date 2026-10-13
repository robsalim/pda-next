//! Точка данных истории.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DataPoint {
    pub timestamp: NaiveDateTime,
    pub value: f64,
}
