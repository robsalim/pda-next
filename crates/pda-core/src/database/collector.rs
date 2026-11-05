//! Буферизованная запись точек данных в фоне с двойной буферизацией.
//!
//! Двойная буферизация (ping-pong): пока writer пишет буфер A в БД, collect()
//! накапливает новые точки в буфер B. Когда writer закончил — буферы меняются
//! ролями. Это позволяет не терять записи во время долгих записей в БД.
//!
//! Историк вызывает `submit_many()` — один lock на всю пачку (не на каждую
//! запись, как в mpsc-канале). Writer забирает буфер swap-ом — один lock на
//! батч, запись в БД идёт без блокировки.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Local};
use parking_lot::Mutex;
use tokio::sync::Notify;

use super::super::config::JournalConfig;
use super::super::core::tag_registry::DuePoint;
use super::db_service::DbWriter;

/// Одна запись: (метка времени, tag_id, значение, качество).
pub type Record = (DateTime<Local>, i64, f64, i32);

/// Колбэк статуса для панели/веба.
pub type OnStatus = Arc<dyn Fn(serde_json::Value) + Send + Sync + 'static>;

/// Двойная буферизация (ping-pong).
///
/// `active` — буфер накопления: в него пишет историк (`push_batch`) и из него
/// writer забирает данные (`take_into`). Забранные точки writer пишет в БД
/// БЕЗ блокировки, а историк в это время копит в новый активный буфер —
/// поэтому пиковая нагрузка не приводит к потере записей.
///
/// `mem::swap` векторов — O(1): ёмкость переходит из руки в руку, аллокаций в
/// установившемся режиме нет. Lock (`parking_lot`, ~20 нс) удерживается только
/// на время swap, никогда через await.
struct DoubleBuffer {
    active: Mutex<Vec<Record>>,
    /// Предельная глубина накопления: защита памяти, когда БД недоступна.
    capacity: usize,
}

impl DoubleBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            active: Mutex::new(Vec::with_capacity(capacity.min(8192))),
            capacity,
        }
    }

    /// Добавить пачку точек данных. Один lock на всю пачку.
    /// Возвращает количество принятых (остальные уходят в dropped).
    fn push_batch(&self, records: &[DuePoint]) -> usize {
        let mut buf = self.active.lock();
        let room = self.capacity.saturating_sub(buf.len());
        let accepted = records.len().min(room);
        for r in &records[..accepted] {
            buf.push((r.timestamp, r.tag_id, r.value, r.quality));
        }
        accepted
    }

    /// Добавить одну запись.
    fn push_one(&self, timestamp: DateTime<Local>, tag_id: i64, value: f64, quality: i32) -> bool {
        let mut buf = self.active.lock();
        if buf.len() >= self.capacity {
            return false;
        }
        buf.push((timestamp, tag_id, value, quality));
        true
    }

    /// Забрать до `limit` записей в `scratch` (освободив место в активном
    /// буфере). Возвращает число забранных. Lock удерживается только здесь.
    ///
    /// ВАЖНО: `scratch` очищается ДО swap. Иначе в активный буфер попадали бы
    /// записи предыдущей пачки (swap меняет содержимое местами), и writer писал
    /// бы их в БД повторно — дубли росли бы с каждой итерацией.
    fn take_into(&self, scratch: &mut Vec<Record>, limit: usize) -> usize {
        let mut buf = self.active.lock();
        scratch.clear();
        if buf.len() <= limit {
            // Весь буфер уходит writer'у; ему достаётся и его ёмкость.
            std::mem::swap(&mut *buf, scratch);
            scratch.len()
        } else {
            scratch.extend(buf.drain(..limit));
            limit
        }
    }

    /// Число точек, ожидающих записи.
    fn len(&self) -> usize {
        self.active.lock().len()
    }
}

/// Очередь записей + двойная буферизация + фоновая задача, которая пишет в базу.
pub struct Collector {
    /// База для истории. Может быть заменена на лету.
    writer: parking_lot::RwLock<Arc<dyn DbWriter>>,
    flush_interval: AtomicU64, // наносекунды
    batch_limit: usize,
    retention_days: AtomicU64,

    /// Двойная буферизация.
    double_buf: Arc<DoubleBuffer>,

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
        Self {
            writer: parking_lot::RwLock::new(writer),
            flush_interval: AtomicU64::new(cfg.journal_flush_ms.max(50) * 1_000_000),
            batch_limit: cfg.journal_batch.max(1),
            retention_days: AtomicU64::new(cfg.retention_days.max(0.0).to_bits()),
            double_buf: Arc::new(DoubleBuffer::new(cfg.journal_queue)),
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
    /// Ставит одно значение в буфер. Не блокируется и не бросает исключений.
    pub fn submit(
        &self,
        timestamp: DateTime<Local>,
        tag_id: i64,
        value: f64,
        quality: i32,
    ) -> bool {
        if self.double_buf.push_one(timestamp, tag_id, value, quality) {
            true
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// Ставит много значений; возвращает сколько принято.
    /// Один lock на всю пачку (в отличие от mpsc — lock на каждую запись).
    pub fn submit_many(&self, records: &[DuePoint]) -> usize {
        let accepted = self.double_buf.push_batch(records);
        if accepted < records.len() {
            self.dropped
                .fetch_add((records.len() - accepted) as u64, Ordering::Relaxed);
        }
        accepted
    }

    /// Требует от писателя слить очередь немедленно.
    pub fn flush_now(&self) {
        self.wake.notify_waiters();
    }

    pub fn set_flush_interval_ms(&self, ms: u64) {
        self.flush_interval
            .store(ms.max(50) * 1_000_000, Ordering::Relaxed);
        self.wake.notify_waiters();
    }

    /// Смена сервиса БД на лету:
    /// накопленная очередь при остановке сольётся уже в новую базу.
    pub fn set_writer(&self, writer: Arc<dyn DbWriter>) {
        *self.writer.write() = writer;
    }

    pub fn set_retention_days(&self, days: f64) {
        self.retention_days
            .store(days.max(0.0).to_bits(), Ordering::Relaxed);
        self.wake.notify_waiters();
    }

    /// Текущий интервал сливания, мс.
    pub fn flush_interval_ms(&self) -> u64 {
        self.flush_interval.load(Ordering::Relaxed) / 1_000_000
    }

    /// Сколько точек ещё не записано.
    pub fn queued(&self) -> usize {
        self.double_buf.len()
    }

    /// Максимум точек в одной пачке INSERT.
    pub fn batch_limit(&self) -> usize {
        self.batch_limit
    }

    /// Статистика для панели/веба.
    pub fn stats_json(&self) -> serde_json::Value {
        serde_json::json!({
            "queued": self.double_buf.len(),
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
        let mut next_purge = if self.retention_days() > 0.0 {
            Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300))
        } else {
            None
        };

        // scratch — вектор для writer'а: swap забирает данные из active,
        // а scratch используется для записи в БД без блокировок.
        let mut scratch = Vec::with_capacity(self.batch_limit);

        loop {
            let timeout = tokio::time::sleep(std::time::Duration::from_nanos(
                self.flush_interval.load(Ordering::Relaxed),
            ));
            tokio::pin!(timeout);

            tokio::select! {
                _ = &mut timeout => {
                    let n = self.double_buf.take_into(&mut scratch, self.batch_limit);
                    if n > 0 {
                        self.write(&scratch).await;
                    }
                }
                _ = self.wake.notified() => {
                    let n = self.double_buf.take_into(&mut scratch, self.batch_limit);
                    if n > 0 {
                        self.write(&scratch).await;
                    }
                }
            }

            if let Some(at) = next_purge {
                if tokio::time::Instant::now() >= at {
                    next_purge =
                        Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
                    self.purge_expired().await;
                }
            } else if self.retention_days() > 0.0 {
                next_purge =
                    Some(tokio::time::Instant::now() + std::time::Duration::from_secs(300));
            }

            if self.stop.load(Ordering::SeqCst) {
                break;
            }
        }

        // При остановке терять накопленное нельзя: сливаем всё до последней точки.
        self.drain_final().await;
    }

    /// Финальный слив буфера (вызывается задачей Historian при остановке).
    pub async fn drain_final(&self) {
        let mut scratch = Vec::with_capacity(self.batch_limit);
        loop {
            scratch.clear();
            let n = self.double_buf.take_into(&mut scratch, usize::MAX);
            if n == 0 {
                break;
            }
            self.write(&scratch).await;
        }
    }

    fn retention_days(&self) -> f64 {
        f64::from_bits(self.retention_days.load(Ordering::Relaxed))
    }

    async fn write(&self, batch: &[Record]) -> usize {
        if batch.is_empty() {
            return 0;
        }
        let writer = self.writer.read().clone();
        match writer.insert_batch(batch).await {
            Ok(_) => {
                self.written
                    .fetch_add(batch.len() as u64, Ordering::Relaxed);
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
