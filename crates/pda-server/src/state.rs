//! AppCore — единое состояние приложения (ядро).
//!
//! Axum забирает Arc<AppCore> в State; Tauri-оболочка (pda-desktop) позже
//! клонирует тот же Arc в tauri::State. Внутри:
//!   db        — сервис БД (он же DbConfigStore и DbWriter),
//!   registry  — живые значения тегов (читается без блокировок менеджера),
//!   manager   — драйверы + журнал (mut-операции только под write-локом),
//!   live_tx   — broadcast-издатель снимков для WebSocket.

use std::path::PathBuf;
use std::sync::Arc;

use pda_core::core::tag_registry::TagRegistry;
use pda_core::database::db_service::{DatabaseService, DbConfig};
use pda_core::drivers::driver_manager::DriverManager;
use tokio::sync::{broadcast, RwLock};

/// Период рассылки live-снимков в WebSocket (мс).
const LIVE_PUSH_MS: u64 = 500;

pub struct AppCore {
    /// Текущий сервис БД. ArcSwap-подобная замена через RwLock: переподключение
    /// меняет его целиком (Python: self.db = DatabaseService(**cfg)).
    pub db: RwLock<Arc<DatabaseService>>,
    pub registry: Arc<TagRegistry>,
    pub manager: RwLock<DriverManager>,
    pub live_tx: broadcast::Sender<String>,
    /// Куда писать db_config.json при переключении БД (тот же каталог, откуда
    /// читали — аналог поведения Python-приложения на месте запуска).
    pub db_config_path: PathBuf,
}

impl AppCore {
    /// Собирает ядро поверх уже открытой базы (путь конфига — по умолчанию).
    pub fn new(db: Arc<DatabaseService>) -> Arc<Self> {
        Self::with_config_path(db, PathBuf::from("db_config.json"))
    }

    pub fn with_config_path(db: Arc<DatabaseService>, db_config_path: PathBuf) -> Arc<Self> {
        let manager = DriverManager::new(db.clone(), db.clone(), None);
        let registry = manager.registry.clone();
        let (live_tx, _) = broadcast::channel(64);
        Arc::new(Self { db: RwLock::new(db), registry, manager: RwLock::new(manager), live_tx, db_config_path })
    }

    /// Открытие БД из db_config.json и сборка ядра.
    pub async fn from_config(cfg: DbConfig, db_config_path: PathBuf) -> Arc<Self> {
        let db = Arc::new(DatabaseService::open(cfg).await);
        Self::with_config_path(db, db_config_path)
    }

    pub async fn db_snapshot(&self) -> Arc<DatabaseService> {
        self.db.read().await.clone()
    }

    /// Переподключение на новую конфигурацию БД (аналог _reconnect_database):
    /// остановить драйверы, открыть новую базу, заменить её в менеджере,
    /// перезапустить. Возвращает ошибку открытия, если новая база недоступна.
    pub async fn switch_database(&self, cfg: DbConfig) -> Result<(), String> {
        let candidate = DatabaseService::open(cfg).await;
        if let Some(err) = &candidate.last_error {
            return Err(err.clone());
        }
        let new_db = Arc::new(candidate);

        {
            let mut mgr = self.manager.write().await;
            mgr.stop_all().await;
            mgr.set_store(new_db.clone());
            mgr.set_db(new_db.clone());
            mgr.start_all().await;
        }
        *self.db.write().await = new_db;
        Ok(())
    }

    /// Фоновая задача: каждые LIVE_PUSH_MS мс отдаёт снимок реестра всем
    /// WebSocket-подписчикам.
    pub fn spawn_live_pump(self: &Arc<Self>) {
        let core = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(LIVE_PUSH_MS));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let snapshot = core.registry.live_snapshot();
                let rows: Vec<serde_json::Value> = snapshot
                    .into_iter()
                    .map(|lv| {
                        serde_json::json!({
                            "id": lv.tag_id,
                            "value": lv.value,
                            "quality": lv.quality,
                            "timestamp": lv.timestamp.map(|t| t.timestamp() as f64),
                        })
                    })
                    .collect();
                let msg = serde_json::json!({ "type": "live", "rows": rows }).to_string();
                // Никто не слушает — не страшно, рассылка для читателей.
                let _ = core.live_tx.send(msg);
            }
        });
    }
}
