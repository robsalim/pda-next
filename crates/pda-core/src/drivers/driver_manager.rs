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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use crate::config::JournalConfig;
use crate::core::historian::Historian;
use crate::core::tag_registry::TagRegistry;
use crate::core::virtual_calc::VirtualPlan;
use crate::database::db_service::{DbConfigStore, DbWriter};
use crate::models::Connection;

use super::base::DriverState;
use super::{modbus_client, modbus_server, priority, registry, snap7, virtual_calc};

/// Запущенный драйвер: состояние + задача (аналог объекта BaseDriver).
struct RunningDriver {
    state: Arc<DriverState>,
    handle: tokio::task::JoinHandle<()>,
}

/// Выделенный рантайм опроса. Драйверы PLC/Modbus крутятся здесь, а не в общем
/// пуле с HTTP/WS/журналом: их потоки поднимаются до HIGHEST, поэтому целевой
/// poll_interval_ms держится даже при загрузке сервера.
fn poll_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pda-poll")
            .enable_all()
            .on_thread_start(priority::set_thread_priority)
            .build()
            .expect("не удалось создать рантайм опроса")
    })
}

/// On-disk формат файла ручных пауз (pda_paused.json рядом с db_config.json).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PauseFile {
    #[serde(default)]
    paused: Vec<i64>,
}

pub struct DriverManager {
    store: Arc<dyn DbConfigStore>,
    pub registry: Arc<TagRegistry>,
    pub historian: Historian,
    drivers: HashMap<i64, RunningDriver>,
    /// Модули, остановленные пользователем вручную (из дерева I/O). Нужны,
    /// чтобы start_drivers/start_all (выход из «только просмотр», старт приложения)
    /// не поднимали то, что оператор намеренно держит выключенным. Флаг
    /// connection.enabled при этом НЕ меняется — это runtime-пауза, а не
    /// правка конфигурации.
    manually_stopped: HashSet<i64>,
    /// Файл для persist пауз (JSON {"paused": [ids]}). Пауза переживает рестарт
    /// процесса и перезагрузку ПК: при старте сервера список перечитывается.
    /// None (тесты ядра) — ничего не пишем и не читаем.
    pause_file: Option<std::path::PathBuf>,
    /// План вычислений виртуальных сигналов (перебирается в reload_tags).
    /// Общая RwLock: задачи-вычислители читают его каждый цикл, менеджер
    /// перезаписывает при любой правке конфигурации тегов/подключений.
    virtual_plan: Arc<std::sync::RwLock<VirtualPlan>>,
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
        Self {
            store,
            registry,
            historian,
            drivers: HashMap::new(),
            manually_stopped: HashSet::new(),
            pause_file: None,
            virtual_plan: Arc::new(std::sync::RwLock::new(VirtualPlan::default())),
        }
    }

    /// Файл, в котором переживают рестарт процесса ручные паузы модулей.
    /// Путь задаётся до первого start_*: существующий список сразу загружается,
    /// и start_all не поднимет то, что оператор остановил в прошлой сессии.
    pub fn set_pause_file(&mut self, path: std::path::PathBuf) {
        match std::fs::read_to_string(&path) {
            Ok(txt) => match serde_json::from_str::<PauseFile>(&txt) {
                Ok(p) => self.manually_stopped = p.paused.into_iter().collect(),
                Err(exc) => tracing::warn!(
                    "[Drivers] {}: не разобран файл пауз ({exc}), начинаем с чистого",
                    path.display()
                ),
            },
            Err(_) => {} // нет файла — это нормально (первый запуск)
        }
        self.pause_file = Some(path);
    }

    /// Сохранить текущий список пауз. Идемпотентно: любая смена manually_stopped
    /// пишет файл целиком (список короткий). Ошибку логируем — она не должна
    /// ломать стоп/старт, только перезапуск без учёта паузы.
    fn save_paused(&self) {
        let Some(path) = &self.pause_file else { return };
        let mut ids: Vec<i64> = self.manually_stopped.iter().copied().collect();
        ids.sort_unstable();
        let txt = serde_json::to_string(&PauseFile { paused: ids }).unwrap_or_default();
        if let Err(exc) = std::fs::write(path, txt) {
            tracing::warn!("[Drivers] Не удалось сохранить файл пауз {}: {exc}", path.display());
        }
    }

    /// Удаление модуля: вычёркиваем его из сохранённых пауз (id больше нет).
    pub fn forget_connection(&mut self, conn_id: i64) {
        if self.manually_stopped.remove(&conn_id) {
            self.save_paused();
        }
    }

    // ---------------------------------------------------------------- tags
    /// Обновляет реестр тегов из БД (старт, изменение конфигурации).
    /// Заодно пересобирает план виртуальных вычислений: правка любого
    /// выражения/id операнда мгновенно отражается на задаче-вычислителе.
    pub async fn reload_tags(&self) {
        let tags = match self.store.get_all_tags().await {
            Ok(t) => t,
            Err(exc) => {
                tracing::warn!("[Drivers] Не удалось загрузить теги в реестр: {exc}");
                return;
            }
        };
        self.registry.replace_all(&tags);

        // Virtual-подключения: план + исключение их точек из журнала.
        let conns = self.store.get_all_connections().await.unwrap_or_default();
        let virt_ids: Vec<i64> = conns
            .iter()
            .filter(|c| registry::is_virtual(&c.driver_type))
            .map(|c| c.id)
            .collect();
        self.registry
            .set_journal_excluded_conns(virt_ids.iter().copied());
        let virt_ids_set: std::collections::HashSet<i64> = virt_ids.iter().copied().collect();
        let virt_tags: Vec<crate::models::Tag> = tags
            .iter()
            .filter(|t| virt_ids_set.contains(&t.connection_id))
            .cloned()
            .collect();
        let plan = VirtualPlan::build(&virt_tags);
        for (id, msg) in &plan.errors {
            tracing::warn!("[Virtual] тег id={id}: {msg}");
        }
        if let Ok(mut g) = self.virtual_plan.write() {
            *g = plan;
        }
    }

    // -------------------------------------------------------------- drivers
    /// Запускает все включённые подключения (уже запущенные пропускаются) и журнал.
    pub async fn start_all(&mut self) {
        self.start_drivers().await;
        self.historian.start().await;
    }

    /// Запускает только драйверы опроса (журнал уже работает).
    /// Используется при выходе из режима «только просмотр».
    pub async fn start_drivers(&mut self) {
        let conns = match self.store.get_all_connections().await {
            Ok(c) => c,
            Err(exc) => {
                tracing::warn!("[Drivers] Не удалось загрузить подключения: {exc}");
                return;
            }
        };

        self.reload_tags().await;
        for c in conns {
            // manually_stopped — пауза, снятая оператором из дерева I/O:
            // такой модуль не поднимаем, даже пока enabled=true.
            if !c.enabled || self.manually_stopped.contains(&c.id) || self.drivers.contains_key(&c.id) {
                continue;
            }
            self.spawn_driver(&c);
        }
    }

    /// Остановить один модуль опроса, не трогая остальные и журнал.
    /// Возвращает true, если драйвер был запущен (или числился вручную
    /// остановленным — повторный stop идемпотентен).
    pub async fn stop_connection(&mut self, conn_id: i64) -> bool {
        self.manually_stopped.insert(conn_id);
        self.save_paused();
        if let Some(d) = self.drivers.remove(&conn_id) {
            d.state.stop();
            // abort дропает future задачи: слушатель Modbus-сервера закрывается
            // сразу, порт освобождается. Handle — из выделенного poll_runtime(),
            // аварить его отсюда (другой рантайм) нельзя, поэтому не ждём.
            d.handle.abort();
            true
        } else {
            // Уже не запущен — пауза всё равно зафиксирована, статус для UI
            // останется «Stopped (paused by operator)».
            true
        }
    }

    /// Снять ручную паузу и запустить модуль из текущей конфигурации.
    /// Возвращает true, если модуль включён и драйвер поднят.
    pub async fn start_connection(&mut self, conn_id: i64) -> bool {
        if self.manually_stopped.remove(&conn_id) {
            self.save_paused();
        }
        if self.drivers.contains_key(&conn_id) {
            return true; // уже работает
        }
        let conns = match self.store.get_all_connections().await {
            Ok(c) => c,
            Err(exc) => {
                tracing::warn!("[Drivers] Не удалось загрузить подключения: {exc}");
                return false;
            }
        };
        let Some(c) = conns.iter().find(|c| c.id == conn_id) else {
            tracing::warn!("[Drivers] Подключение id={conn_id} не найдено");
            return false;
        };
        if !c.enabled {
            tracing::warn!("[Drivers] Подключение '{}' выключено в конфигурации", c.name);
            return false;
        }
        self.reload_tags().await;
        self.spawn_driver(c);
        true
    }

    /// Модули, остановленные вручную (для статуса: чем показывать паузу в UI).
    pub fn manually_stopped_ids(&self) -> Vec<i64> {
        self.manually_stopped.iter().copied().collect()
    }

    pub fn is_manually_stopped(&self, conn_id: i64) -> bool {
        self.manually_stopped.contains(&conn_id)
    }

    /// Запуск без опроса: реестр из БД + журнал. Это режим «только просмотр» —
    /// приложение читает историю, но не опрашивает ПЛК и не занимает порты.
    pub async fn start_journal(&mut self) {
        self.reload_tags().await;
        self.historian.start().await;
    }

    /// Останавливает только драйверы опроса; журнал продолжает работать и
    /// дописывает накопленные точки. Порты (Modbus-сервер) освобождаются.
    pub async fn stop_drivers(&mut self) {
        for (_id, d) in self.drivers.drain() {
            d.state.stop();
            d.handle.abort();
        }
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
        // Modbus-серверу нужен прямой доступ к очереди журнала: записанное
        // Master'ом значение должно попадать в БД при приходе посылки.
        let journal = self.historian.collector().clone();
        let vplan = self.virtual_plan.clone();
        // Опрос — на выделенном рантайме с повышенным приоритетом потоков.
        let handle = poll_runtime().spawn(async move {
            match dt.as_str() {
                "snap7" | "snap7_s200" => snap7::run_snap7(conn_cfg, registry, task_state).await,
                "modbus_client" => {
                    modbus_client::run_modbus_client(conn_cfg, registry, task_state).await
                }
                "modbus_server" => {
                    modbus_server::run_modbus_server(conn_cfg, registry, task_state, Some(journal))
                        .await
                }
                "virtual" => {
                    virtual_calc::run_virtual(conn_cfg, registry, task_state, vplan).await
                }
                _ => {}
            }
        });
        self.drivers.insert(c.id, RunningDriver { state, handle });
    }

    /// Останавливает драйверы, затем журнал: накопленные точки сольются в БД.
    pub async fn stop_all(&mut self) {
        self.stop_drivers().await;
        self.historian.stop().await;
    }

    /// Остановить — пауза 0.3 с — запустить (аналог restart_all).
    pub async fn restart_all(&mut self) {
        self.stop_all().await;
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.start_all().await;
    }

    /// Точечный перезапуск одного подключения (изменение конфигурации).
    /// Конфиг менялся — оператор ожидает работы, снимаем ручную паузу.
    pub async fn restart_connection(&mut self, conn_id: i64) {
        if self.manually_stopped.remove(&conn_id) {
            self.save_paused();
        }
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
        serde_json::json!(self
            .drivers
            .values()
            .map(|d| d.state.to_json())
            .collect::<Vec<_>>())
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
    /// Порт параметризован: tokio-тесты идут в параллель, и два мода на одном
    /// порту дают "Address in use" — bind второго просто не поднимается.
    fn stub_conn_on(port: u16) -> Connection {
        let mut cfg = ConnConfig::new();
        cfg.insert("host".into(), serde_json::json!("127.0.0.1"));
        cfg.insert("port".into(), serde_json::json!(port));
        Connection::new(7, "cut", "modbus_server", true, 1000, cfg)
    }

    fn stub_conn() -> Connection {
        stub_conn_on(15020)
    }

    /// Заглушка конфигурации: одно подключение + один тег.
    struct StoreStub {
        port: u16,
    }

    impl StoreStub {
        fn new(port: u16) -> Self {
            Self { port }
        }
    }

    #[async_trait]
    impl DbConfigStore for StoreStub {
        async fn get_all_connections(&self) -> DbResult<Vec<Connection>> {
            Ok(vec![stub_conn_on(self.port)])
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
            Ok(vec![Tag::new(
                1,
                7,
                "t",
                "C1",
                "BOOL",
                1.0,
                0.0,
                "",
                "Общие",
                0,
                0.0,
            )])
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
        async fn move_tags(&self, _ids: &[i64], _c: i64, _g: &str) -> DbResult<()> {
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
            Arc::new(StoreStub::new(15020)),
            Arc::new(WriterStub {
                written: written.clone(),
            }),
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
        assert!(
            mgr.driver_status(7).starts_with("Running"),
            "статус: {}",
            mgr.driver_status(7)
        );
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

    /// Ручная пауза модуля: stop_connection снимает драйвер, start_drivers
    /// (пересборка всех) его НЕ поднимает, start_connection — поднимает снова.
    #[tokio::test]
    async fn manual_stop_start_connection() {
        let mut mgr = DriverManager::new(
            Arc::new(StoreStub::new(15021)),
            Arc::new(WriterStub {
                written: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Some(JournalConfig::default()),
        );

        // Поднимаем один модуль (id=7 из StoreStub) и ждём Running.
        assert!(mgr.start_connection(7).await);
        for _ in 0..50 {
            if mgr.running_count() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(mgr.running_count(), 1, "модуль запущен start_connection");

        // Останавливаем вручную — драйвер уходит, статус Stopped.
        assert!(mgr.stop_connection(7).await);
        assert_eq!(mgr.running_count(), 0, "после stop_connection ничего не опрашивается");
        assert_eq!(mgr.driver_status(7), "Stopped");

        // start_drivers (общий пересбор) НЕ должен поднимать вручную остановленный.
        mgr.start_drivers().await;
        assert_eq!(mgr.running_count(), 0, "manually_stopped переживает start_drivers");
        assert!(mgr.is_manually_stopped(7));

        // Снятие паузы — снова работает.
        assert!(mgr.start_connection(7).await);
        assert!(!mgr.is_manually_stopped(7));
        for _ in 0..50 {
            if mgr.running_count() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(mgr.running_count(), 1, "start_connection после паузы поднимает модуль");

        mgr.stop_all().await;
    }

    /// Ручная пауза переживает рестарт процесса: список пишется в pause-файл,
    /// новый DriverManager с тем же файлом не поднимает остановленный модуль,
    /// а start_connection чистит и память, и файл.
    #[tokio::test]
    async fn manual_pause_survives_restart() {
        let dir = std::env::temp_dir().join(format!("pda_pause_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let pause_path = dir.join("pda_paused.json");
        let _ = std::fs::remove_file(&pause_path);

        let mk = || {
            let mut m = DriverManager::new(
                Arc::new(StoreStub::new(15023)),
                Arc::new(WriterStub {
                    written: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
                Some(JournalConfig::default()),
            );
            m.set_pause_file(pause_path.clone());
            m
        };

        // Сессия 1: старт → ручная пауза → файл создан, в нём id=7.
        let mut m1 = mk();
        assert!(m1.start_connection(7).await);
        for _ in 0..50 {
            if m1.running_count() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(m1.stop_connection(7).await);
        m1.stop_all().await; // "смерть процесса": драйверы сняты
        drop(m1);
        let txt = std::fs::read_to_string(&pause_path).expect("pause-файл записан");
        assert!(txt.contains("7"), "в файле id остановленного модуля: {txt}");

        // Сессия 2: новый менеджер с тем же файлом — модуль остаётся в паузе.
        let mut m2 = mk();
        assert!(m2.is_manually_stopped(7), "пауза загружена из файла");
        m2.start_all().await;
        assert_eq!(m2.running_count(), 0, "start_all не поднимает paused-модуль");

        // Снятие паузы чистит и набор, и файл.
        assert!(m2.start_connection(7).await);
        let txt = std::fs::read_to_string(&pause_path).unwrap();
        assert!(!txt.contains("7"), "после start в файле пусто: {txt}");
        m2.stop_all().await;

        // forget_connection убирает следы удалённого модуля из файла.
        assert!(m2.stop_connection(7).await);
        m2.forget_connection(7);
        let txt = std::fs::read_to_string(&pause_path).unwrap();
        assert!(!txt.contains("7"), "удалённый модуль вычеркнут: {txt}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_driver_skipped() {
        let cfg = JournalConfig::default();
        let mut mgr = DriverManager::new(
            Arc::new(StoreStub::new(15022)),
            Arc::new(WriterStub {
                written: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
            Some(cfg),
        );
        // неизвестный тип не должен паниковать
        let c = Connection::new(8, "x", "nosuch", true, 1000, Default::default());
        mgr.spawn_driver(&c);
        assert!(mgr.drivers.is_empty());
    }
}
