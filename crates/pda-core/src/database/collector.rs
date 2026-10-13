//! Буферизованная запись точек данных в фоне.
//!
//! Драйверы опроса складывают значения в очередь, а отдельная фоновая задача
//! пачками пишет их в базу. Такое разделение нужно, чтобы запись не блокировала
//! цикл опроса: время ожидания БД перестаёт влиять на периодичность опроса,
//! а кратковременное пропадание базы не роняет опрос.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Local};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::sync::Notify;

use super::super::config::JournalConfig;
use super::super::core::tag_registry::DuePoint;
use super::db_service::DbWriter;

/// Одна запись: (метка времени, tag_id, значение, качество).
pub type Record = (DateTime<Local>, i64, f64, i32);

/// Колбэк статуса для панели/веба.
pub type OnStatus = Arc<dyn Fn(serde_json::Value) + Send + Sync + 'static>;

/// Очередь записей и фоновая задача, которая пишет их в базу.
pub struct Collector {
    /// База для истории. Может быть заменена на лету —
    /// поэтому RwLock, а не фиксированный Arc.
    writer: parking_lot::RwLock<Arc<dyn DbWriter>>,
    flush_interval: AtomicU64, // наносекунды (потокобезопасная настройка на лету)
    batch_limit: usize,
    retention_days: AtomicU64, // ф64 в битовом виде; 0 = хранить вечно

    tx: Mutex<Option<mpsc::Sender<Record>>>,
    rx: Mutex<Option<mpsc::Receiver<Record>>>,

    queued: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,

    written: Arc<AtomicU64>,
    batches: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
    last_error: Arc<Mutex<String>>,
    last_flush: Arc<Mutex<Option<DateTime<Local>>>>,
    last_purge: Arc<Mutex<Option<DateTime<Local>>>>,
    on_status: Option<OnStatus>,
}

impl Collector {
    pub fn new(writer: Arc<dyn DbWriter>, cfg: &JournalConfig) -> Self {
        let (tx, rx) = mpsc::channel(cfg.journal_queue.max(16));
        Self {
            writer: parking_lot::RwLock::new(writer),
            flush_interval: AtomicU64::new(cfg.journal_flush_ms.max(50) * 1_000_000),
            batch_limit: cfg.journal_batch.max(1),
            retention_days: AtomicU64::new(cfg.retention_days.max(0.0).to_bits()),
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(Some(rx)),
            queued: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            handle: tokio::sync::Mutex::new(None),
            written: Arc::new(AtomicU64::new(0)),
            batches: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            errors: Arc::new(AtomicU64::new(0)),
            last_error: Arc::new(Mutex::new(String::new())),
            last_flush: Arc::new(Mutex::new(None)),
            last_purge: Arc::new(Mutex::new(None)),
            on_status: None,
        }
    }

    pub fn with_status(mut self, on_status: OnStatus) -> Self {
        self.on_status = Some(on_status);
        self
    }

    // ------------------------------------------------------------------ life
    pub fn start(self: Arc<Self>) {
        let mut guard = match self.handle.try_lock() {
            Ok(g) => g,
            Err(_) => return, // уже запущен (stop() ещё не отдал lock)
        };
        if guard.is_some() {
            return;
        }
        self.stop.store(false, Ordering::SeqCst);

        let this = self.clone();
        *guard = Some(tokio::spawn(async move { this.run().await }));
    }

    /// Останавливает писателя, предварительно слив накопленное в базу.
    pub async fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        // Забираем handle через timeout: поток сам сольёт очередь при выходе
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), h).await;
        }
    }

    pub fn running(&self) -> bool {
        self.handle.try_lock().map(|g| g.is_some()).unwrap_or(true)
    }

    // ------------------------------------------------------------------ api
    /// Ставит одно значение в очередь. Не блокируется и не бросает исключений.
    pub fn submit(&self, timestamp: DateTime<Local>, tag_id: i64, value: f64, quality: i32) -> bool {
        let tx = self.tx.lock();
        let Some(tx) = tx.as_ref() else { return false };
        match tx.try_send((timestamp, tag_id, value, quality)) {
            Ok(()) => {
                self.queued.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Ставит много значений; возвращает сколько принято.
    pub fn submit_many(&self, records: &[DuePoint]) -> usize {
        let mut accepted = 0;
        for r in records {
            if self.submit(r.timestamp, r.tag_id, r.value, r.quality) {
                accepted += 1;
            }
        }
        accepted
    }

    /// Требует от писателя слить очередь немедленно.
    pub fn flush_now(&self) {
        self.wake.notify_waiters();
    }

    pub fn set_flush_interval_ms(&self, ms: u64) {
        self.flush_interval.store(ms.max(50) * 1_000_000, Ordering::Relaxed);
        self.wake.notify_waiters();
    }

    /// Смена сервиса БД на лету:
    /// накопленная очередь при остановке сольётся уже в новую базу.
    pub fn set_writer(&self, writer: Arc<dyn DbWriter>) {
        *self.writer.write() = writer;
    }

    pub fn set_retention_days(&self, days: f64) {
        self.retention_days.store(days.max(0.0).to_bits(), Ordering::Relaxed);
        self.wake.notify_waiters();
    }

    /// Текущий интервал сливания, мс.
    pub fn flush_interval_ms(&self) -> u64 {
        self.flush_interval.load(Ordering::Relaxed) / 1_000_000
    }

    /// Статистика для панели/веба.
    pub fn stats_json(&self) -> serde_json::Value {
        serde_json::json!({
            "queued": self.queued.load(Ordering::Relaxed),
            "written": self.written.load(Ordering::Relaxed),
            "batches": self.batches.load(Ordering::Relaxed),
            "dropped": self.dropped.load(Ordering::Relaxed),
            "errors": self.errors.load(Ordering::Relaxed),
            "last_error": self.last_error.lock().clone(),
            "last_flush": self.last_flush.lock().map(|t| t.to_rfc3339()),
            "last_purge": self.last_purge.lock().map(|t| t.to_rfc3339()),
            "flush_interval_ms": self.flush_interval_ms(),
            "retention_days": f64::from_bits(self.retention_days.load(Ordering::Relaxed)),
        })
    }

    // ------------------------------------------------------------------ work
    async fn run(&self) {
        // Писатель держит rx; submit() работает через tx
        let mut rx = match self.rx.lock().take() {
            Some(rx) => rx,
            None => return, // уже запущен повторно — не должно случиться
        };

        let mut next_purge = if self.retention_days() > 0.0 {
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300))
        } else {
            None
        };

        loop {
            let timeout = tokio::time::sleep(std::time::Duration::from_nanos(
                self.flush_interval.load(Ordering::Relaxed),
            ));
            tokio::pin!(timeout);

            tokio::select! {
                maybe = rx.recv() => {
                    // Пришла первая запись — собираем пачку и пишем
                    let mut batch: Vec<Record> = Vec::with_capacity(64);
                    if let Some(first) = maybe {
                        self.queued.fetch_sub(1, Ordering::Relaxed);
                        batch.push(first);
                        while batch.len() < self.batch_limit {
                            match rx.try_recv() {
                                Ok(r) => { self.queued.fetch_sub(1, Ordering::Relaxed); batch.push(r); }
                                Err(_) => break,
                            }
                        }
                        self.write(batch).await;
                    }
                }
                _ = &mut timeout, if self.queued.load(Ordering::Relaxed) > 0 => {
                    let batch = self.drain(&mut rx, self.batch_limit);
                    if !batch.is_empty() {
                        self.write(batch).await;
                    }
                }
                _ = self.wake.notified() => {
                    let batch = self.drain(&mut rx, self.batch_limit);
                    if !batch.is_empty() {
                        self.write(batch).await;
                    }
                }
            }

            if let Some(at) = next_purge {
                if tokio::time::Instant::now() >= at {
                    next_purge = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
                    self.purge_expired().await;
                }
            } else if self.retention_days() > 0.0 {
                next_purge = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
            }

            if self.stop.load(Ordering::SeqCst) {
                break;
            }
        }

        // При остановке терять накопленное нельзя: сливаем всё до последней точки.
        self.drain_final_with(&mut rx).await;
        // Возвращаем приёмник в слот: иначе повторный start() найдёт None,
        // и после первого restart_all писатель уже никогда не оживёт
        // (submit() молча упрётся в закрытый канал — история перестанет писаться).
        *self.rx.lock() = Some(rx);
    }

    fn drain(&self, rx: &mut mpsc::Receiver<Record>, limit: usize) -> Vec<Record> {
        let mut batch = Vec::with_capacity(limit.min(64));
        while batch.len() < limit {
            match rx.try_recv() {
                Ok(r) => {
                    self.queued.fetch_sub(1, Ordering::Relaxed);
                    batch.push(r);
                }
                Err(_) => break,
            }
        }
        batch
    }

    /// Финальный слив очереди (вызывается задачей Historian при остановке).
    pub async fn drain_final(&self) {
        // Guard не должен переживать await — иначе future не Send для tokio::spawn
        let rx = self.rx.lock().take();
        if let Some(mut rx) = rx {
            self.drain_final_with(&mut rx).await;
        }
    }

    async fn drain_final_with(&self, rx: &mut mpsc::Receiver<Record>) {
        loop {
            let batch = self.drain(rx, self.batch_limit.saturating_mul(20).max(1));
            if batch.is_empty() {
                break;
            }
            self.write(batch).await;
        }
    }

    fn retention_days(&self) -> f64 {
        f64::from_bits(self.retention_days.load(Ordering::Relaxed))
    }

    async fn write(&self, batch: Vec<Record>) -> usize {
        if batch.is_empty() {
            return 0;
        }
        let writer = self.writer.read().clone();
        match writer.insert_batch(&batch).await {
            Ok(_) => {
                self.written.fetch_add(batch.len() as u64, Ordering::Relaxed);
                self.batches.fetch_add(1, Ordering::Relaxed);
                *self.last_flush.lock() = Some(Local::now());
                self.publish();
                batch.len()
            }
            Err(err) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                *self.last_error.lock() = err.chars().take(200).collect();
                0
            }
        }
    }

    /// Удаляет точки старше срока хранения. 0 = хранить вечно.
    pub async fn purge_expired(&self) -> usize {
        let days = self.retention_days();
        if days <= 0.0 {
            return 0;
        }
        let ms = (days * 86_400_000.0) as i64;
        let cutoff = Local::now() - chrono::Duration::milliseconds(ms);
        let writer = self.writer.read().clone();
        match writer.purge_before(cutoff).await {
            Ok(removed) => {
                *self.last_purge.lock() = Some(Local::now());
                if removed > 0 {
                    self.publish();
                }
                removed
            }
            Err(err) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                *self.last_error.lock() = err.chars().take(200).collect();
                0
            }
        }
    }

    fn publish(&self) {
        if let Some(cb) = &self.on_status {
            cb(self.stats_json());
        }
    }
}
