//! Тесты конвейера журнала (Collector + Historian) с мок-писателем БД.
//! Внешние сервисы не требуют — всё in-memory.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Duration, Local};

use pda_core::config::JournalConfig;
use pda_core::core::tag_registry::{TagRegistry, DuePoint};
use pda_core::database::collector::{Collector, Record};
use pda_core::database::db_service::DbWriter;
use pda_core::models::Engine;

struct MockWriter {
    inserted: Mutex<Vec<Record>>,
    purged_before: Mutex<Option<chrono::DateTime<Local>>>,
    written_count: AtomicU64,
    fail_insert: std::sync::atomic::AtomicBool,
}

impl MockWriter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inserted: Mutex::new(Vec::new()),
            purged_before: Mutex::new(None),
            written_count: AtomicU64::new(0),
            fail_insert: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn inserted(&self) -> Vec<Record> {
        self.inserted.lock().unwrap().clone()
    }
}

#[async_trait]
impl DbWriter for MockWriter {
    async fn insert_batch(&self, records: &[Record]) -> Result<usize, String> {
        if self.fail_insert.load(Ordering::Relaxed) {
            return Err("mock insert error".into());
        }
        let mut buf = self.inserted.lock().unwrap();
        let before = buf.len();
        buf.extend_from_slice(records);
        self.written_count.fetch_add((buf.len() - before) as u64, Ordering::Relaxed);
        Ok(records.len())
    }
    async fn purge_before(&self, cutoff: chrono::DateTime<Local>) -> Result<usize, String> {
        *self.purged_before.lock().unwrap() = Some(cutoff);
        Ok(0)
    }
    fn engine(&self) -> Engine {
        Engine::Sqlite
    }
}

fn cfg() -> JournalConfig {
    JournalConfig {
        journal_scan_ms: 20,
        journal_flush_ms: 50,
        journal_batch: 10,
        journal_queue: 100,
        retention_days: 0.0,
    }
}

fn due_point(tag_id: i64, value: f64, ts: chrono::DateTime<Local>) -> DuePoint {
    DuePoint {
        timestamp: ts,
        tag_id,
        value,
        quality: 0,
    }
}

fn now() -> chrono::DateTime<Local> {
    Local::now()
}

#[tokio::test]
async fn collector_accepts_and_drops_on_capacity() {
    let writer = MockWriter::new();
    let coll = Arc::new(Collector::new(writer.clone(), &cfg()));
    coll.clone().start();

    let pts: Vec<_> = (0..5i64).map(|i| due_point(i, i as f64, now())).collect();
    let accepted = coll.submit_many(&pts);
    assert_eq!(accepted, 5);
    assert_eq!(coll.queued(), 5);
    assert_eq!(writer.inserted().len(), 0);

    coll.flush_now();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    assert_eq!(coll.queued(), 0, "очередь должна опустеть после слива в БД");
    assert_eq!(writer.inserted().len(), 5, "все 5 точек должны уйти в мок-БД");

#[tokio::test]
async fn collector_write_error_increments_errors() {
    let writer = MockWriter::new();
    writer.fail_insert.store(true, Ordering::Relaxed);
    let coll = Collector::new(writer.clone(), &cfg());

    let pts: Vec<_> = (0..3i64).map(|i| due_point(i, i as f64, now())).collect();
    coll.submit_many(&pts);
    coll.flush_now();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert_eq!(coll.stats_json()["errors"].as_u64(), Some(1));
    assert_eq!(coll.queued(), 3);
}

#[tokio::test]
async fn collector_submit_one_and_batch_limit() {
    let writer = MockWriter::new();
    let coll = Collector::new(writer.clone(), &cfg());
    assert_eq!(coll.batch_limit(), 10);
    assert!(coll.submit(now(), 1, 42.0, 0));
    assert_eq!(coll.queued(), 1);
}

#[tokio::test]
async fn collector_purge_expired_calls_writer() {
    let writer = MockWriter::new();
    let coll = Collector::new(writer.clone(), &cfg());
    coll.set_retention_days(1.0);
    let cutoff = now() - Duration::days(2);
    let removed = coll.purge_expired().await;
    assert_eq!(removed, 0);
    assert_eq!(*writer.purged_before.lock().unwrap(), Some(cutoff));
}

#[tokio::test]
async fn collector_set_writer_swaps_database() {
    let w1 = MockWriter::new();
    let coll = Collector::new(w1.clone(), &cfg());
    coll.submit_many(&[due_point(1, 1.0, now())]);
    coll.flush_now();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(w1.inserted().len(), 1);

    let w2 = MockWriter::new();
    coll.set_writer(w2.clone());
    coll.submit_many(&[due_point(2, 2.0, now())]);
    coll.flush_now();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(w2.inserted().len(), 1);
    assert_eq!(w1.inserted().len(), 1);
}

#[tokio::test]
async fn historian_selects_and_writes_through_mock() {
    let registry = Arc::new(TagRegistry::new());
    let writer = MockWriter::new();
    let cfg = cfg();
    let hist = pda_core::core::historian::Historian::new(registry.clone(), writer.clone(), &cfg);

    let tag = pda_core::models::Tag::new(
        1, 1, "t1", "HR100", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0,
    );
    registry.replace_all(&[tag]);
    registry.update_value(1, Some(77.0), 0, None);

    hist.start();
    hist.wake_scan();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let inserted = writer.inserted();
    assert!(
        inserted.iter().any(|r| r.1 == 1 && (r.2 - 77.0).abs() < f64::EPSILON),
        "точка 77.0 для тега 1 должна попасть в БД: {inserted:?}"
    );
    hist.stop().await;
}

#[tokio::test]
async fn historian_stats_contains_selected() {
    let registry = Arc::new(TagRegistry::new());
    let writer = MockWriter::new();
    let hist = pda_core::core::historian::Historian::new(registry, writer, &cfg());
    let stats = hist.stats();
    assert!(stats.get("scan_interval_ms").is_some());
    assert!(stats.get("selected").is_some());
}
    assert_eq!(coll.queued(), 0);
    assert_eq!(writer.inserted().len(), 5);
}

#[tokio::test]
async fn collector_dropped_counter_on_overflow() {
    let writer = MockWriter::new();
    let coll = Collector::new(writer.clone(), &cfg());

    let pts: Vec<_> = (0..250i64).map(|i| due_point(i, i as f64, now())).collect();
    let accepted = coll.submit_many(&pts);
    assert_eq!(accepted, 100);
    assert_eq!(coll.queued(), 100);
    assert_eq!(coll.stats_json()["dropped"].as_u64(), Some(150));
}