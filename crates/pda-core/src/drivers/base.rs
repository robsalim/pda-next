//! Общая база драйверов.
//!
//! Здесь состояние вынесено в `DriverState` (статус/метрики для GUI и веба),
//! а цикл опроса у каждого драйвера свой — логика
//! соединений и планов чтения у S7 и Modbus различается.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use parking_lot::Mutex;

use crate::core::tag_point::{TagPoint, QUALITY_GOOD};
use crate::core::tag_registry::{now_mono, TagRegistry};
use crate::models::Connection;

/// Скользящее среднее метрик цикла опроса.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Stats {
    pub read_ms: f64,
    pub db_ms: f64,
    pub cycle_ms: f64,
    pub requests: usize,
    pub tags: usize,
    pub hz: f64,
}

/// Разделяемое состояние одного драйвера: читается DriverManager'ом,
/// GUI и веб-сервером, пишется задачей опроса.
pub struct DriverState {
    pub conn_id: i64,
    pub conn_name: String,
    pub driver_type: String,
    pub running: AtomicBool,
    pub status: Mutex<String>,
    pub stats: Mutex<Stats>,
    pub last_error: Mutex<Option<String>>,
}

impl DriverState {
    pub fn new(conn: &Connection) -> Arc<Self> {
        Arc::new(Self {
            conn_id: conn.id,
            conn_name: conn.name.clone(),
            driver_type: conn.driver_type.clone(),
            running: AtomicBool::new(false),
            status: Mutex::new("Stopped".into()),
            stats: Mutex::new(Stats::default()),
            last_error: Mutex::new(None),
        })
    }

    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock() = s.into();
    }

    pub fn status(&self) -> String {
        self.status.lock().clone()
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        *self.status.lock() = "Stopped".into();
    }

    /// Снимок для API/панели.
    pub fn to_json(&self) -> serde_json::Value {
        let stats = self.stats.lock();
        serde_json::json!({
            "conn_id": self.conn_id,
            "name": self.conn_name,
            "driver_type": self.driver_type,
            "running": self.running.load(Ordering::Relaxed),
            "status": self.status.lock().clone(),
            "last_error": self.last_error.lock().clone(),
            "stats": *stats,
        })
    }
}

/// Сглаживание метрик по EMA (k = 0.2):
/// первый цикл пишется без сглаживания.
pub fn update_stats(state: &DriverState, cycle_s: f64, read_s: f64, db_s: f64, requests: usize, tags: usize) {
    const K: f64 = 0.2;
    let mut s = state.stats.lock();
    let first = s.cycle_ms == 0.0;
    let ema = |prev: f64, next: f64| if first { next } else { prev * (1.0 - K) + next * K };
    s.read_ms = ema(s.read_ms, read_s * 1000.0);
    s.db_ms = ema(s.db_ms, db_s * 1000.0);
    s.cycle_ms = ema(s.cycle_ms, cycle_s * 1000.0);
    s.requests = requests;
    s.tags = tags;
    s.hz = if cycle_s > 0.0 { 1.0 / cycle_s } else { 0.0 };
}

/// Строка статуса цикла опроса:
/// `Polling · 12 сигн./3 запр. · 8.4 мс/цикл (119 Гц)`.
pub fn fmt_poll_status(state: &DriverState, period: Duration, cycle_s: f64) -> String {
    let s = state.stats.lock();
    let over = if cycle_s > period.as_secs_f64() * 1.05 { " ПЕРЕГРУЗ" } else { "" };
    format!(
        "Polling · {} сигн./{} запр. · {:.1} мс/цикл ({:.0} Гц){}",
        s.tags, s.requests, s.cycle_ms, s.hz, over
    )
}

/// Кэш списка тегов подключения (ttl = 2.0 с).
/// С реестром обращение к БД в цикле опроса вовсе не нужен.
pub struct TagCache {
    pub ttl: f64,
    pub at: f64,
    pub points: Vec<Arc<TagPoint>>,
}

impl TagCache {
    pub fn new() -> Self {
        Self { ttl: 2.0, at: 0.0, points: Vec::new() }
    }

    pub fn invalidate(&mut self) {
        self.at = 0.0;
        self.points.clear();
    }

    pub fn get(&mut self, registry: &TagRegistry, conn_id: i64) -> Vec<Arc<TagPoint>> {
        let now = now_mono();
        if self.points.is_empty() || (now - self.at) >= self.ttl {
            self.points = registry.points_for(conn_id);
            self.at = now;
        }
        self.points.clone()
    }
}

impl Default for TagCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Драйвер вызывает её с парами {tag_id: raw_value}. Масштабирование и
/// публикация живого состояния — на стороне реестра, а в историю пишет
/// журнал по своим правилам (COV + log_interval_ms), поэтому цикл опроса
/// не зависит от скорости базы.
pub fn submit_values(registry: &TagRegistry, values: &HashMap<i64, f64>, ts: DateTime<Local>) {
    for (tid, raw) in values {
        registry.update_value(*tid, Some(*raw), QUALITY_GOOD, Some(ts));
    }
}

/// Теги, не прочитанные в этом цикле, — плохое качество.
pub fn mark_cycle_errors(registry: &TagRegistry, ids: impl IntoIterator<Item = i64>) {
    registry.mark_bad(ids);
}

/// Целевой ПЕРИОД цикла: poll_interval_ms, но не меньше 1 мс.
pub fn poll_period(conn: &Connection) -> Duration {
    Duration::from_millis(conn.poll_interval_ms.max(1) as u64)
}

/// Подпись адреса для конструктора в интерфейсе (hint, области, примеры) —
/// единая форма json для всех драйверов.
pub fn address_json(
    hint: &str,
    areas: &[(&str, &str)],
    examples: &[(&str, &str, &str)],
) -> serde_json::Value {
    serde_json::json!({
        "hint": hint,
        "areas": areas.iter().map(|(a, d)| [*a, *d]).collect::<Vec<_>>(),
        "examples": examples.iter().map(|(a, t, d)| [*a, *t, *d]).collect::<Vec<_>>(),
    })
}
