//! Асинхронный журнал: реестр тегов -> очередь -> фоновая запись в БД.
//!
//! Поток опроса (драйвер) только обновляет TagPoint в реестре; задача журнала
//! раз в scan_interval выбирает подлежащие записи (COV + мёртвая зона +
//! log_interval_ms) и ставит их в ограниченную очередь; писатель (Collector)
//! пачками пишет очередь в БД.
//!
//! Если БД недоступна или отстаёт, точки теряются только при переполнении
//! очереди — это фиксируется счётчиком dropped. Опрос при этом не блокируется.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use chrono::Local;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::super::config::JournalConfig;
use super::super::database::collector::Collector;
use super::super::database::db_service::DbWriter;
use super::tag_registry::TagRegistry;

/// Период скана журнала, не реже 20 мс.
fn scan_interval_ms(cfg: &JournalConfig) -> u64 {
    cfg.journal_scan_ms.max(20)
}

pub struct Historian {
    registry: Arc<TagRegistry>,
    collector: Arc<Collector>,
    scan_ms: u64,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    points_selected: Arc<AtomicU64>,
    handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Historian {
    pub fn new(
        registry: Arc<TagRegistry>,
        writer: Arc<dyn DbWriter>,
        cfg: &JournalConfig,
    ) -> Self {
        Self {
            registry,
            collector: Arc::new(Collector::new(writer, cfg)),
            scan_ms: scan_interval_ms(cfg),
            stop: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            points_selected: Arc::new(AtomicU64::new(0)),
            handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Интервал скана (для статуса).
    pub fn scan_interval_ms(&self) -> u64 {
        self.scan_ms
    }

    /// Доступ к писателю для управления извне (flush, настройки, статистика).
    pub fn collector(&self) -> &Arc<Collector> {
        &self.collector
    }

    pub async fn start(&self) {
        let mut guard = self.handle.lock().await;
        if guard.is_some() {
            return;
        }
        self.collector.clone().start();
        self.stop.store(false, Ordering::SeqCst);

        let registry = self.registry.clone();
        let collector = self.collector.clone();
        let stop = self.stop.clone();
        let wake = self.wake.clone();
        let scan_ms = self.scan_ms;
        let selected = self.points_selected.clone();

        *guard = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(scan_ms));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = wake.notified() => {}
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let started = std::time::Instant::now();
                let now = Local::now();
                let due = registry.collect_due(now);
                if !due.is_empty() {
                    selected.fetch_add(due.len() as u64, Ordering::Relaxed);
                    collector.submit_many(&due);
                }
                // Периодически тряхнём очередь, чтобы данные уходили в БД вовремя
                collector.flush_now();

                // Пауза до следующего скана с учётом времени самого скана
                let elapsed = started.elapsed().as_millis() as u64;
                let sleep_ms = scan_ms.saturating_sub(elapsed);
                if sleep_ms > 0 {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)) => {}
                        _ = wake.notified() => {}
                    }
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
            // При остановке терять накопленное нельзя: сливаем всё до последней точки
            collector.drain_final().await;
        }));
    }

    /// Останавливает сканер и писателя. Писатель сам сольёт остаток очереди.
    pub async fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h).await;
        }
        self.collector.stop().await;
    }

    /// Статистика журнала (для панели и веба).
    pub fn stats(&self) -> serde_json::Value {
        let mut s = self.collector.stats_json();
        if let Some(o) = s.as_object_mut() {
            o.insert("selected".into(), serde_json::json!(self.points_selected.load(Ordering::Relaxed)));
            o.insert("scan_interval_ms".into(), serde_json::json!(self.scan_ms));
        }
        s
    }

    /// Подать сигнал пробуждения сканера (используется тестами и при reload).
    pub fn wake_scan(&self) {
        self.wake.notify_waiters();
    }
}
