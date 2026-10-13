//! Менеджер драйверов (аналог drivers/driver_manager.py).
//!
//! Владелец runtime-состояния тегов:
//!   registry  — единый реестр tag_id -> TagPoint, куда драйверы пишут показания;
//!   historian — фоновый журнал: выборка подлежащих точек + пакетная запись в БД.
//!
//! UI и веб-сервер читают текущие значения из registry, не трогая историю.
//! В Python каждый драйвер — объект с потоком; здесь драйвер — tokio-задача,
//! а её наблюдаемое состояние живёт в `DriverState` (её и останавливаем снятием
//! флага `running`, как `stop()` в оригинале).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::config::JournalConfig;
use crate::core::historian::Historian;
use crate::core::tag_registry::TagRegistry;
use crate::database::db_service::{DbConfigStore, DbWriter};
use crate::models::Connection;

use super::base::DriverState;
use super::{modbus_client, modbus_server, registry, snap7};

/// Запущенный драйвер: состояние + задача (аналог объекта BaseDriver).
struct RunningDriver {
    state: Arc<DriverState>,
    handle: tokio::task::JoinHandle<()>,
}

pub struct DriverManager {
    store: Arc<dyn DbConfigStore>,
    pub registry: Arc<TagRegistry>,
    pub historian: Historian,
    drivers: HashMap<i64, RunningDriver>,
}

impl DriverManager {
    /// store — конфигурация (подключения/теги), writer — история. В Python это
    /// один и тот же DatabaseService; cfg переопределяет JOURNAL_CONFIG.
    pub fn new(
        store: Arc<dyn DbConfigStore>,
        writer: Arc<dyn DbWriter>,
        cfg: Option<JournalConfig>,
    ) -> Self {
        let jcfg = cfg.unwrap_or_default();
        let registry = Arc::new(TagRegistry::new());
        let historian = Historian::new(registry.clone(), writer, &jcfg);
        Self { store, registry, historian, drivers: HashMap::new() }
    }

    // ---------------------------------------------------------------- tags
    /// Обновляет реестр тегов из БД (старт, изменение конфигурации).
    pub async fn reload_tags(&self) {
        match self.store.get_all_tags().await {
            Ok(tags) => self.registry.replace_all(&tags),
            Err(exc) => {
                tracing::warn!("[Drivers] Не удалось загрузить теги в реестр: {exc}");
            }
        }
    }

    // -------------------------------------------------------------- drivers
    /// Запускает все включённые подключения (уже запущенные пропускаются) и журнал.
    pub async fn start_all(&mut self) {
        let conns = match self.store.get_all_connections().await {
            Ok(c) => c,
            Err(exc) => {
                tracing::warn!("[Drivers] Не удалось загрузить подключения: {exc}");
                return;
            }
        };

        self.reload_tags().await;
        for c in conns {
            if !c.enabled || self.drivers.contains_key(&c.id) {
                continue;
            }
            self.spawn_driver(&c);
        }

        self.historian.start().await;
    }

    /// Аналог `driver_class(c, db, registry).start()` по каноническому типу.
    fn spawn_driver(&mut self, c: &Connection) {
        let dt = registry::normalize_driver_type(&c.driver_type);
        if !registry::is_known(&dt) {
            tracing::warn!(
                "[Drivers] Неизвестный тип драйвера '{}' у подключения '{}' (id={}), пропущено",
                c.driver_type,
                c.name,
                c.id
            );
            return;
        }

        let conn_cfg = Arc::new(c.clone());
        let state = DriverState::new(c);
        let registry = self.registry.clone();
        let task_state = state.clone();
        let handle = tokio::spawn(async move {
            match dt.as_str() {
                "snap7" | "snap7_s200" => snap7::run_snap7(conn_cfg, registry, task_state).await,
                "modbus_client" => modbus_client::run_modbus_client(conn_cfg, registry, task_state).await,
                "modbus_server" => modbus_server::run_modbus_server(conn_cfg, registry, task_state).await,
                _ => {}
            }
        });
        self.drivers.insert(c.id, RunningDriver { state, handle });
    }

    /// Останавливает драйверы, затем журнал: накопленные точки сольются в БД.
    pub async fn stop_all(&mut self) {
        for (_id, d) in self.drivers.drain() {
            d.state.stop();
            d.handle.abort();
        }
        self.historian.stop().await;
    }

    /// Остановить — пауза 0.3 с — запустить (аналог restart_all).
    pub async fn restart_all(&mut self) {
        self.stop_all().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.start_all().await;
    }

    /// Точечный перезапуск одного подключения (изменение конфигурации).
    pub async fn restart_connection(&mut self, conn_id: i64) {
        if let Some(d) = self.drivers.remove(&conn_id) {
            d.state.stop();
            d.handle.abort();
        }
        match self.store.get_all_connections().await {
            Ok(conns) => {
                self.reload_tags().await;
                if let Some(c) = conns.iter().find(|c| c.id == conn_id && c.enabled) {
                    self.spawn_driver(c);
                }
            }
            Err(exc) => tracing::warn!("[Drivers] Не удалось загрузить подключения: {exc}"),
        }
    }

    // ------------------------------------------------------------------ misc
    /// Смена сервиса БД (переподключение): журнал должен писать в новую базу.
    /// Драйверы публикуют значения в реестр, а не пишут в БД напрямую, поэтому
    /// достаточно заменить писателя (Python: historian.collector.db = ...).
    pub fn set_db(&self, writer: Arc<dyn DbWriter>) {
        self.historian.collector().set_writer(writer);
    }

    /// Смена источника конфигурации вместе с базой (переподключение БД):
    /// следующие reload_tags/start_all возьмут подключения из нового store.
    pub fn set_store(&mut self, store: Arc<dyn DbConfigStore>) {
        self.store = store;
    }

    /// Настройки retention журнала на лету.
    pub fn set_retention_days(&self, days: f64) {
        self.historian.collector().set_retention_days(days);
    }

    pub fn journal_stats(&self) -> serde_json::Value {
        self.historian.stats()
    }

    /// Статус драйвера подключения ("Stopped", если его нет).
    pub fn driver_status(&self, conn_id: i64) -> String {
        self.drivers
            .get(&conn_id)
            .map(|d| d.state.status())
            .unwrap_or_else(|| "Stopped".into())
    }

    /// Состояния всех драйверов для панели/веба.
    pub fn drivers_json(&self) -> serde_json::Value {
        serde_json::json!(
            self.drivers.values().map(|d| d.state.to_json()).collect::<Vec<_>>()
        )
    }

    pub fn running_count(&self) -> usize {
        self.drivers
            .values()
            .filter(|d| d.state.running.load(Ordering::Relaxed))
            .count()
    }

    pub fn store(&self) -> &Arc<dyn DbConfigStore> {
        &self.store
    }
}

// -------------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tag_registry::TagRegistry;
    use crate::database::collector::Record;
    use crate::database::db_service::{DbError, DbResult};
    use crate::models::{ConnConfig, DataPoint, Tag};
    use async_trait::async_trait;
    use chrono::Local;

    /// Соединение слушаем на свободном высокопортовом адресе: 502 требует
    /// прав администратора, а тест не должен от них зависеть.
    fn stub_conn() -> Connection {
        let mut cfg = ConnConfig::new();
        cfg.insert("host".into(), serde_json::json!("127.0.0.1"));
        cfg.insert("port".into(), serde_json::json!(15020));
        Connection::new(7, "cut", "modbus_server", true, 1000, cfg)
    }

    /// Заглушка конфигурации: одно подключение + один тег.
    struct StoreStub;

    #[async_trait]
    impl DbConfigStore for StoreStub {
        async fn get_all_connections(&self) -> DbResult<Vec<Connection>> {
            Ok(vec![stub_conn()])
        }
        async fn add_connection(&self, _c: &Connection) -> DbResult<i64> {
            Ok(7)
        }
        async fn update_connection(&self, _c: &Connection) -> DbResult<()> {
            Ok(())
        }
        async fn delete_connection(&self, _id: i64) -> DbResult<()> {
            Ok(())
        }
        async fn get_all_tags(&self) -> DbResult<Vec<Tag>> {
            Ok(vec![Tag::new(1, 7, "t", "C1", "BOOL", 1.0, 0.0, "", "Общие", 0, 0.0)])
        }
        async fn get_configured_tags(&self) -> DbResult<Vec<Tag>> {
            self.get_all_tags().await
        }
        async fn get_tags_by_connection(&self, _id: i64) -> DbResult<Vec<Tag>> {
            self.get_all_tags().await
        }
        async fn add_tag(&self, _t: &Tag) -> DbResult<i64> {
            Ok(1)
        }
        async fn update_tag(&self, _t: &Tag) -> DbResult<()> {
            Ok(())
        }
        async fn delete_tags(&self, _ids: &[i64]) -> DbResult<()> {
            Ok(())
        }
        async fn get_groups_by_connection(&self, _id: i64) -> DbResult<Vec<String>> {
            Ok(vec![])
        }
        async fn add_group(&self, _c: i64, _n: &str) -> DbResult<()> {
            Ok(())
        }
        async fn rename_group(&self, _c: i64, _o: &str, _n: &str) -> DbResult<()> {
            Ok(())
        }
        async fn delete_group(&self, _c: i64, _n: &str, _w: bool) -> DbResult<()> {
            Ok(())
        }
        async fn get_group_counts(&self, _id: i64) -> DbResult<Vec<(String, usize)>> {
            Ok(vec![])
        }
        async fn get_data_points_range(
            &self,
            _t: i64,
            _s: chrono::DateTime<Local>,
            _e: chrono::DateTime<Local>,
        ) -> DbResult<Vec<DataPoint>> {
            Ok(vec![])
        }
        async fn get_data_points_by_span(
            &self,
            _t: i64,
            _d: i64,
        ) -> DbResult<(Vec<DataPoint>, f64, f64)> {
            Ok((vec![], 0.0, 0.0))
        }
    }

    /// Писатель-заглушка: считает записанные точки.
    struct WriterStub {
        written: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl DbWriter for WriterStub {
        async fn insert_batch(&self, records: &[Record]) -> DbResult<usize> {
            self.written.fetch_add(records.len(), Ordering::Relaxed);
            Ok(records.len())
        }
        async fn purge_before(&self, _c: chrono::DateTime<Local>) -> DbResult<usize> {
            Ok(0)
        }
        fn engine(&self) -> crate::models::Engine {
            crate::models::Engine::Sqlite
        }
    }

    #[tokio::test]
    async fn start_stop_cycle() {
        let written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cfg = JournalConfig {
            journal_scan_ms: 20,
            journal_flush_ms: 50,
            journal_batch: 10,
            journal_queue: 100,
            retention_days: 0.0,
        };
        let mut mgr = DriverManager::new(
            Arc::new(StoreStub),
            Arc::new(WriterStub { written: written.clone() }),
            Some(cfg),
        );

        mgr.start_all().await;
        assert_eq!(mgr.registry.len(), 1, "теги загружены в реестр");
        // bind асинхронный: ждём, пока задача переведёт статус в Running (Port ...)
        for _ in 0..50 {
            if mgr.running_count() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(mgr.running_count(), 1, "подключение modbus_server запущено");
        assert!(mgr.driver_status(7).starts_with("Running"), "статус: {}", mgr.driver_status(7));
        assert_eq!(mgr.driver_status(999), "Stopped", "нет драйвера — Stopped");

        // Пауза на один цикл журнала, потом остановка — остаток сольётся в stub
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        mgr.stop_all().await;
        assert_eq!(mgr.running_count(), 0);
        // без внешних writes история может остаться пустой — не проверяем written
        let _ = written.load(Ordering::Relaxed);

        let stats = mgr.journal_stats();
        assert!(stats.get("written").is_some(), "статистика журнала есть");
    }

    #[tokio::test]
    async fn unknown_driver_skipped() {
        let cfg = JournalConfig::default();
        let mut mgr = DriverManager::new(
            Arc::new(StoreStub),
            Arc::new(WriterStub { written: Arc::new(std::sync::atomic::AtomicUsize::new(0)) }),
            Some(cfg),
        );
        // неизвестный тип не должен паниковать
        let c = Connection::new(8, "x", "nosuch", true, 1000, Default::default());
        mgr.spawn_driver(&c);
        assert!(mgr.drivers.is_empty());
    }
}
