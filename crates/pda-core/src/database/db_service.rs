//! Сервис базы данных.
//!
//! Поддерживает SQLite (WAL) и PostgreSQL (опционально TimescaleDB).
//!
//! Интерфейс разделён на два трейта:
//!  - DbWriter  — только запись истории (нужен Collector/Historian);
//!  - DbConfigStore — чтение/изменение конфигурации (нужен main/GUI/веб).
//!
//! Запросы SQLite выполняются под async-мьютексом соединения: операции
//! короткие (WAL, пачечная вставка); писатель — отдельная задача,
//! GUI-запросы идут тем же соединением.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use rusqlite::{params, Connection};
use tokio::sync::Mutex as AsyncMutex;
use tokio_postgres::types as pg_types;

use super::super::models::{Connection as ConnModel, DataPoint, Engine, Tag};
use super::collector::Record;

pub type DbError = String;
pub type DbResult<T> = Result<T, DbError>;

/// Конфигурация подключения к БД (db_config.json).
#[derive(Debug, Clone)]
pub struct DbConfig {
    pub engine: Engine,
    pub sqlite_path: String,
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: String,
}

impl Default for DbConfig {
    /// По умолчанию — SQLite.
    fn default() -> Self {
        Self {
            engine: Engine::Sqlite,
            sqlite_path: "data/pda_local.db".into(),
            host: "localhost".into(),
            port: 5432,
            dbname: "pda_data".into(),
            user: "postgres".into(),
            password: "25052015".into(),
        }
    }
}

impl DbConfig {
    /// Собирает конфиг из JSON (db_config.json).
    pub fn from_json(v: &serde_json::Value) -> Self {
        let mut cfg = DbConfig::default();
        match v.get("engine").and_then(|e| e.as_str()) {
            Some("postgres") => cfg.engine = Engine::Postgres,
            Some("mysql") => cfg.engine = Engine::Mysql,
            Some("mariadb") => cfg.engine = Engine::Mariadb,
            _ => cfg.engine = Engine::Sqlite,
        }
        if let Some(s) = v.get("sqlite_path").and_then(|x| x.as_str()) {
            cfg.sqlite_path = s.to_string();
        }
        if let Some(s) = v.get("host").and_then(|x| x.as_str()) {
            cfg.host = s.to_string();
        }
        if let Some(p) = v.get("port").and_then(|x| x.as_i64()) {
            cfg.port = p as u16;
        }
        if let Some(s) = v.get("dbname").and_then(|x| x.as_str()) {
            cfg.dbname = s.to_string();
        }
        if let Some(s) = v.get("user").and_then(|x| x.as_str()) {
            cfg.user = s.to_string();
        }
        if let Some(s) = v.get("password").and_then(|x| x.as_str()) {
            cfg.password = s.to_string();
        }
        cfg
    }

    /// В виде JSON (для db_config.json и ответов API).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": match self.engine {
                Engine::Sqlite => "sqlite",
                Engine::Postgres => "postgres",
                Engine::Mysql => "mysql",
                Engine::Mariadb => "mariadb",
            },
            "sqlite_path": self.sqlite_path,
            "host": self.host,
            "port": self.port,
            "dbname": self.dbname,
            "user": self.user,
        })
    }
}

/// Текст datetime для SQLite. Разделитель — пробел: так пишет значения
/// сам sqlite3 (адаптер datetime), а строковое сравнение с 'T'-форматом
/// isoformat() ломает окна меньше суток (' ' < 'T' лексикографически).
pub fn dt_str(dt: &DateTime<Local>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

fn parse_sqlite_ts(s: &str) -> Option<DateTime<Local>> {
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
        .ok()?;
    Local.from_local_datetime(&naive).single().or_else(|| Local.from_local_datetime(&naive).earliest()).map(|d| d.into())
}

/// Нарезка под лимит количества параметров одного запроса.
fn chunks<T>(items: &[T], size: usize) -> Vec<&[T]> {
    items.chunks(size.max(1)).collect()
}

// -------------------------------------------------------------------- трейты
/// Только запись истории — то, что нужно Collector.
#[async_trait]
pub trait DbWriter: Send + Sync {
    async fn insert_batch(&self, records: &[Record]) -> DbResult<usize>;
    async fn purge_before(&self, cutoff: DateTime<Local>) -> DbResult<usize>;
    fn engine(&self) -> Engine;
    /// Есть ли в data_points колонка quality (создаётся миграцией схемы).
    fn has_quality(&self) -> bool {
        true
    }
}

/// CRUD конфигурации — то, что нужно main/GUI/вебу.
#[async_trait]
pub trait DbConfigStore: Send + Sync {
    async fn get_all_connections(&self) -> DbResult<Vec<ConnModel>>;
    async fn add_connection(&self, conn: &ConnModel) -> DbResult<i64>;
    /// Изменение существующего подключения (name/driver/enabled/period/config).
    async fn update_connection(&self, conn: &ConnModel) -> DbResult<()>;
    async fn delete_connection(&self, conn_id: i64) -> DbResult<()>;
    async fn get_all_tags(&self) -> DbResult<Vec<Tag>>;
    async fn get_configured_tags(&self) -> DbResult<Vec<Tag>>;
    async fn get_tags_by_connection(&self, conn_id: i64) -> DbResult<Vec<Tag>>;
    async fn add_tag(&self, tag: &Tag) -> DbResult<i64>;
    async fn update_tag(&self, tag: &Tag) -> DbResult<()>;
    async fn delete_tag(&self, tag_id: i64) -> DbResult<()> {
        self.delete_tags(&[tag_id]).await
    }
    async fn delete_tags(&self, tag_ids: &[i64]) -> DbResult<()>;
    /// Перенос переменных в другую группу/модуль (drag&drop в дереве I/O,
    /// аналог move_tags_to_group из io_widget.py).
    async fn move_tags(&self, tag_ids: &[i64], target_conn_id: i64, target_group: &str) -> DbResult<()>;
    async fn get_groups_by_connection(&self, conn_id: i64) -> DbResult<Vec<String>>;
    async fn add_group(&self, conn_id: i64, name: &str) -> DbResult<()>;
    async fn rename_group(&self, conn_id: i64, old_name: &str, new_name: &str) -> DbResult<()>;
    async fn delete_group(&self, conn_id: i64, name: &str, with_tags: bool) -> DbResult<()>;
    async fn get_group_counts(&self, conn_id: i64) -> DbResult<Vec<(String, usize)>>;
    async fn get_data_points_range(
        &self,
        tag_id: i64,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> DbResult<Vec<DataPoint>>;
    async fn get_data_points_by_span(
        &self,
        tag_id: i64,
        delta_ms: i64,
    ) -> DbResult<(Vec<DataPoint>, f64, f64)>;
}

// ---------------------------------------------------------------- DatabaseService
#[derive(Clone)]
enum Backend {
    Sqlite(Arc<AsyncMutex<Connection>>),
    Postgres(Arc<AsyncMutex<tokio_postgres::Client>>),
}

/// Сервис БД. Если открытие не удалось, backend = None, а причина в last_error
/// (allow_offline-режим).
pub struct DatabaseService {
    cfg: DbConfig,
    backend: Option<Backend>,
    pub is_available: bool,
    pub last_error: Option<DbError>,
}

impl DatabaseService {
    /// Открытие БД с созданием схемы.
    pub async fn open(cfg: DbConfig) -> Self {
        match cfg.engine {
            Engine::Sqlite => Self::open_sqlite(cfg).await,
            Engine::Postgres => Self::open_postgres(cfg).await,
            Engine::Mysql | Engine::Mariadb => Self::unavailable(
                cfg,
                "MySQL/MariaDB в Rust-версии ещё не перенесён (используйте SQLite или PostgreSQL)".into(),
            ),
        }
    }

    fn unavailable(cfg: DbConfig, err: DbError) -> Self {
        Self { cfg, backend: None, is_available: false, last_error: Some(err) }
    }

    /// Создаёт целевую PostgreSQL-базу, подключаясь к служебной базе 'postgres'.
    /// Возвращает true, если база создана, false — если уже существовала.
    pub async fn create_postgres_database(cfg: &DbConfig) -> Result<bool, DbError> {
        if cfg.dbname.trim().is_empty() {
            return Err("Не указано имя базы данных".into());
        }
        let (client, conn_task) = tokio_postgres::Config::new()
            .host(&cfg.host)
            .port(cfg.port)
            .dbname("postgres")
            .user(&cfg.user)
            .password(&cfg.password)
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| format!("Не удалось подключиться к базе 'postgres': {e}"))?;
        tokio::spawn(async move {
            if let Err(e) = conn_task.await {
                tracing::error!("postgres (create) connection error: {e}");
            }
        });
        let rows = client
            .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&cfg.dbname])
            .await
            .map_err(|e| format!("Не удалось проверить наличие базы: {e}"))?;
        if !rows.is_empty() {
            return Ok(false);
        }
        // Имя базы нельзя параметризовать — экранируем двойными кавычками.
        let ident = format!("\"{}\"", cfg.dbname.replace('"', "\"\""));
        client
            .batch_execute(&format!("CREATE DATABASE {ident}"))
            .await
            .map_err(|e| format!("CREATE DATABASE: {e}"))?;
        Ok(true)
    }

    async fn open_sqlite(cfg: DbConfig) -> Self {
        let path = cfg.sqlite_path.clone();
        let opened = tokio::task::spawn_blocking(move || -> Result<Connection, DbError> {
            if let Some(parent) = Path::new(&path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        format!(
                            "Нет доступа к папке SQLite-базы '{}': {e}. \
                             Путь в db_config.json мог остаться с другого компьютера — \
                             укажите существующий каталог, например data/pda_local.db",
                            parent.display()
                        )
                    })?;
                }
            }
            let conn = Connection::open(&path).map_err(|e| format!("SQLite '{path}': {e}"))?;
            // WAL + NORMAL позволяют фону писать пачками, не блокируя
            // читателей (GUI/веб) на каждой транзакции
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;")
                .map_err(|e| format!("PRAGMA: {e}"))?;
            ensure_sqlite_schema(&conn).map_err(|e| format!("схема SQLite: {e}"))?;
            Ok(conn)
        })
        .await;
        match opened {
            Ok(Ok(conn)) => Self {
                cfg,
                backend: Some(Backend::Sqlite(Arc::new(AsyncMutex::new(conn)))),
                is_available: true,
                last_error: None,
            },
            Ok(Err(err)) => Self::unavailable(cfg, err),
            Err(err) => Self::unavailable(cfg, err.to_string()),
        }
    }

    async fn open_postgres(cfg: DbConfig) -> Self {
        let attempt = tokio_postgres::Config::new()
            .host(&cfg.host)
            .port(cfg.port)
            .dbname(&cfg.dbname)
            .user(&cfg.user)
            .password(&cfg.password)
            .connect(tokio_postgres::NoTls)
            .await;
        match attempt {
            Ok((client, conn_task)) => {
                tokio::spawn(async move {
                    if let Err(e) = conn_task.await {
                        tracing::error!("postgres connection error: {e}");
                    }
                });
                let client = Arc::new(AsyncMutex::new(client));
                // Таблицы создаём отдельной транзакцией: раньше из-за общей
                // try/except сбой create_hypertable откатывал создание таблиц.
                if let Err(err) = ensure_postgres_schema(&client).await {
                    return Self::unavailable(cfg, err);
                }
                try_create_timescale(&client).await;
                Self { cfg, backend: Some(Backend::Postgres(client)), is_available: true, last_error: None }
            }
            Err(err) => {
                // Понятная диагностика ошибки подключения.
                let tcp = tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port)).await;
                let msg = if tcp.is_err() {
                    format!(
                        "Сервер PostgreSQL {}:{} не отвечает ({}. Проверьте, запущена ли служба и правильность порта.",
                        cfg.host, cfg.port, err
                    )
                } else {
                    format!(
                        "Сервер {}:{} отверг подключение: {} (база '{}', пользователь '{}')",
                        cfg.host, cfg.port, err, cfg.dbname, cfg.user
                    )
                };
                Self::unavailable(cfg, msg)
            }
        }
    }

    /// Бэкенд, если БД доступна.
    fn be(&self) -> DbResult<Backend> {
        self.backend.clone().ok_or_else(|| "База данных недоступна".to_string())
    }

    pub fn engine(&self) -> Engine {
        self.cfg.engine
    }

    pub fn config(&self) -> &DbConfig {
        &self.cfg
    }
}

// --------------------------------------------------------------- схемы таблиц
fn ensure_sqlite_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS connections (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            driver_type TEXT NOT NULL,
            enabled INTEGER DEFAULT 1,
            poll_interval_ms INTEGER DEFAULT 100,
            config TEXT NOT NULL DEFAULT '{}'
        );
        CREATE TABLE IF NOT EXISTS tag_groups (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            connection_id INTEGER REFERENCES connections(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            UNIQUE(connection_id, name)
        );
        CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            connection_id INTEGER REFERENCES connections(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            address_str TEXT NOT NULL,
            data_type TEXT NOT NULL DEFAULT 'FLOAT',
            scale REAL DEFAULT 1.0,
            offset_val REAL DEFAULT 0.0,
            unit TEXT DEFAULT '',
            group_name TEXT DEFAULT 'Общие',
            log_interval_ms INTEGER DEFAULT 1000,
            deadband REAL DEFAULT 0.0
        );
        CREATE TABLE IF NOT EXISTS data_points (
            timestamp TEXT NOT NULL,
            tag_id INTEGER NOT NULL,
            value REAL NOT NULL,
            quality INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_dp_ts ON data_points(tag_id, timestamp DESC);
        ",
    )?;
    // Миграция старых баз: добавляем колонки регистрации, если их нет
    let has_col = |table: &str, col: &str| -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table});"))?;
        let mut names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        while let Some(n) = names.next() {
            if n? == col {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !has_col("tags", "log_interval_ms")? {
        conn.execute("ALTER TABLE tags ADD COLUMN log_interval_ms INTEGER DEFAULT 1000;", [])?;
    }
    if !has_col("tags", "deadband")? {
        conn.execute("ALTER TABLE tags ADD COLUMN deadband REAL DEFAULT 0.0;", [])?;
    }
    if !has_col("data_points", "quality")? {
        conn.execute("ALTER TABLE data_points ADD COLUMN quality INTEGER DEFAULT 0;", [])?;
    }
    Ok(())
}

async fn ensure_postgres_schema(client: &AsyncMutex<tokio_postgres::Client>) -> DbResult<()> {
    let c = client.lock().await;
    c.batch_execute(
        "
        CREATE TABLE IF NOT EXISTS connections (
            id SERIAL PRIMARY KEY,
            name VARCHAR(100) NOT NULL,
            driver_type VARCHAR(50) NOT NULL,
            enabled BOOLEAN DEFAULT TRUE,
            poll_interval_ms INT DEFAULT 100,
            config JSONB NOT NULL DEFAULT '{}'
        );
        CREATE TABLE IF NOT EXISTS tag_groups (
            id SERIAL PRIMARY KEY,
            connection_id INT REFERENCES connections(id) ON DELETE CASCADE,
            name VARCHAR(100) NOT NULL,
            UNIQUE(connection_id, name)
        );
        CREATE TABLE IF NOT EXISTS tags (
            id SERIAL PRIMARY KEY,
            connection_id INT REFERENCES connections(id) ON DELETE CASCADE,
            name VARCHAR(100) NOT NULL,
            address_str VARCHAR(100) NOT NULL,
            data_type VARCHAR(20) NOT NULL DEFAULT 'FLOAT',
            scale DOUBLE PRECISION DEFAULT 1.0,
            offset_val DOUBLE PRECISION DEFAULT 0.0,
            unit VARCHAR(20) DEFAULT '',
            group_name VARCHAR(100) DEFAULT 'Общие',
            log_interval_ms INT DEFAULT 1000,
            deadband DOUBLE PRECISION DEFAULT 0.0
        );
        CREATE TABLE IF NOT EXISTS data_points (
            \"timestamp\" TIMESTAMPTZ NOT NULL,
            tag_id INT NOT NULL,
            value DOUBLE PRECISION NOT NULL,
            quality SMALLINT DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_dp_tag_ts ON data_points (tag_id, \"timestamp\" DESC);
        ALTER TABLE tags ADD COLUMN IF NOT EXISTS log_interval_ms INT DEFAULT 1000;
        ALTER TABLE tags ADD COLUMN IF NOT EXISTS deadband DOUBLE PRECISION DEFAULT 0.0;
        ALTER TABLE data_points ADD COLUMN IF NOT EXISTS quality SMALLINT DEFAULT 0;
        ",
    )
    .await
    .map_err(|e| format!("схема PostgreSQL: {e}"))?;
    Ok(())
}

/// TimescaleDB опционален: без него data_points остаётся обычной таблицей.
async fn try_create_timescale(client: &AsyncMutex<tokio_postgres::Client>) {
    let c = client.lock().await;
    let ext = c.batch_execute("CREATE EXTENSION IF NOT EXISTS timescaledb;").await;
    if ext.is_err() {
        tracing::info!("TimescaleDB не доступен (data_points остаётся обычной таблицей)");
        return;
    }
    let h = c
        .batch_execute("SELECT create_hypertable('data_points', 'timestamp', if_not_exists => TRUE);")
        .await;
    if let Err(e) = h {
        tracing::info!("TimescaleDB hypertable не создан: {e}");
    }
}

// --------------------------------------------------------------- строки -> Tag
/// Колонки SELECT для тегов.
const TAG_SELECT: &str = "id, connection_id, name, address_str, data_type, scale, offset_val, \
     unit, COALESCE(group_name, 'Общие') as group_name, \
     COALESCE(log_interval_ms, 1000) as log_interval_ms, \
     COALESCE(deadband, 0.0) as deadband";

const TAG_SELECT_T: &str = "t.id, t.connection_id, t.name, t.address_str, t.data_type, t.scale, t.offset_val, \
     t.unit, COALESCE(t.group_name, 'Общие') as group_name, \
     COALESCE(t.log_interval_ms, 1000) as log_interval_ms, \
     COALESCE(t.deadband, 0.0) as deadband";

fn tag_from_irow(r: &rusqlite::Row) -> rusqlite::Result<Tag> {
    Ok(Tag {
        id: r.get(0)?,
        connection_id: r.get(1)?,
        name: r.get(2)?,
        address_str: r.get(3)?,
        data_type: r.get(4)?,
        scale: r.get::<_, Option<f64>>(5)?.unwrap_or(1.0),
        offset_val: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
        unit: r.get::<_, Option<String>>(7)?.unwrap_or_default(),
        group_name: r.get::<_, Option<String>>(8)?.unwrap_or_else(|| "Общие".into()),
        log_interval_ms: r.get::<_, Option<i64>>(9)?.unwrap_or(1000),
        deadband: r.get::<_, Option<f64>>(10)?.unwrap_or(0.0),
    })
}

fn tag_from_prow(r: &tokio_postgres::Row) -> Tag {
    Tag {
        id: r.get::<_, i32>(0) as i64,
        connection_id: r.get::<_, i32>(1) as i64,
        name: r.get(2),
        address_str: r.get(3),
        data_type: r.get(4),
        scale: r.get::<_, Option<f64>>(5).unwrap_or(1.0),
        offset_val: r.get::<_, Option<f64>>(6).unwrap_or(0.0),
        unit: r.get::<_, Option<String>>(7).unwrap_or_default(),
        group_name: r.get::<_, Option<String>>(8).unwrap_or_else(|| "Общие".into()),
        log_interval_ms: r.get::<_, i32>(9) as i64,
        deadband: r.get::<_, Option<f64>>(10).unwrap_or(0.0),
    }
}

fn conn_from_irow(r: &rusqlite::Row) -> rusqlite::Result<ConnModel> {
    let id: i64 = r.get(0)?;
    let name: String = r.get(1)?;
    let driver_type: String = r.get(2)?;
    let enabled: i64 = r.get(3)?;
    let poll: i64 = r.get(4)?;
    let config: String = r.get(5)?;
    let cfg = serde_json::from_str(&config).unwrap_or_else(|_| default_config_json());
    Ok(ConnModel {
        id,
        name,
        driver_type: crate::drivers::registry::normalize_driver_type(&driver_type),
        enabled: enabled != 0,
        poll_interval_ms: poll,
        config: cfg,
    })
}

fn conn_from_prow(r: &tokio_postgres::Row) -> ConnModel {
    let cfg_val: serde_json::Value = r
        .try_get::<_, serde_json::Value>(5)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    ConnModel {
        id: r.get::<_, i32>(0) as i64,
        name: r.get(1),
        driver_type: crate::drivers::registry::normalize_driver_type(r.get(2)),
        enabled: r.get::<_, bool>(3),
        poll_interval_ms: r.get::<_, i32>(4) as i64,
        config: match cfg_val {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => default_config_json(),
        },
    }
}

fn default_config_json() -> std::collections::HashMap<String, serde_json::Value> {
    Default::default()
}

// ------------------------------------------------------------------- DbWriter
#[async_trait]
impl DbWriter for DatabaseService {
    fn engine(&self) -> Engine {
        self.cfg.engine
    }

    /// Принимает (timestamp, tag_id, value, quality) — качество всегда есть.
    async fn insert_batch(&self, records: &[Record]) -> DbResult<usize> {
        if records.is_empty() || !self.is_available {
            return Ok(0);
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let mut conn = lock.lock().await;
                // Одна транзакция на батч: без явной транзакции rusqlite делает
                // отдельный fsync на каждую строку — на батче из сотен точек
                // это тормозит в разы.
                let tx = conn.transaction().map_err(|e| e.to_string())?;
                {
                    let mut stmt = tx
                        .prepare_cached(
                            "INSERT INTO data_points (timestamp, tag_id, value, quality) VALUES (?1, ?2, ?3, ?4);",
                        )
                        .map_err(|e| e.to_string())?;
                    for (ts, tag_id, value, quality) in records {
                        stmt.execute(params![dt_str(ts), tag_id, value, quality])
                            .map_err(|e| e.to_string())?;
                    }
                }
                tx.commit().map_err(|e| e.to_string())?;
                Ok(records.len())
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                // Один запрос на пачку.
                let mut sql =
                    String::from("INSERT INTO data_points (\"timestamp\", tag_id, value, quality) VALUES ");
                // Колонки: TIMESTAMPTZ, INT, DOUBLE PRECISION, SMALLINT.
                // tokio-postgres сериализует параметр строго под тип, который ждёт
                // сервер: i64 принимает только INT8, i32 — только INT4. Без кастов
                // каждая пачка падала с "error serializing parameter 1" (tag_id i64 -> INT).
                let utc_times: Vec<DateTime<Utc>> =
                    records.iter().map(|(ts, ..)| ts.with_timezone(&Utc)).collect();
                let tag_ids: Vec<i32> = records.iter().map(|(_, t, ..)| *t as i32).collect();
                let qualities: Vec<i16> = records.iter().map(|(_, _, _, q)| *q as i16).collect();
                let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::with_capacity(records.len() * 4);
                for (i, (_, _, value, _)) in records.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    let base = i * 4;
                    sql.push_str(&format!("(${}, ${}, ${}, ${})", base + 1, base + 2, base + 3, base + 4));
                    args.push(&utc_times[i]);
                    args.push(&tag_ids[i]);
                    args.push(value);
                    args.push(&qualities[i]);
                }
                c.execute(&sql, &args).await.map_err(|e| e.to_string())?;
                Ok(records.len())
            }
        }
    }

    /// Удаляет точки старше срока хранения. 0 = хранить вечно.
    async fn purge_before(&self, cutoff: DateTime<Local>) -> DbResult<usize> {
        if !self.is_available {
            return Ok(0);
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let removed = conn
                    .execute("DELETE FROM data_points WHERE timestamp < ?1;", params![dt_str(&cutoff)])
                    .map_err(|e| e.to_string())?;
                Ok(removed)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let removed = c
                    .execute(
                        "DELETE FROM data_points WHERE \"timestamp\" < $1;",
                        &[&cutoff.with_timezone(&Utc)],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(removed as usize)
            }
        }
    }
}

// -------------------------------------------------------------- DbConfigStore
#[async_trait]
impl DbConfigStore for DatabaseService {
    async fn get_all_connections(&self) -> DbResult<Vec<ConnModel>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let mut stmt = conn
                    .prepare_cached("SELECT id, name, driver_type, enabled, poll_interval_ms, config FROM connections ORDER BY id ASC;")
                    .map_err(|e| e.to_string())?;
                let rows = stmt.query_map([], conn_from_irow).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let rows = c
                    .query(
                        "SELECT id, name, driver_type, enabled, poll_interval_ms, config FROM connections ORDER BY id ASC;",
                        &[],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(rows.iter().map(conn_from_prow).collect())
            }
        }
    }

    async fn add_connection(&self, conn: &ConnModel) -> DbResult<i64> {
        match self.be()? {
            Backend::Sqlite(lock) => {
                let cfg_json = serde_json::to_string(&conn.config).map_err(|e| e.to_string())?;
                let conn_db = lock.lock().await;
                conn_db
                    .execute(
                        "INSERT INTO connections (name, driver_type, enabled, poll_interval_ms, config) VALUES (?1, ?2, ?3, ?4, ?5);",
                        params![conn.name, conn.driver_type, conn.enabled as i64, conn.poll_interval_ms, cfg_json],
                    )
                    .map_err(|e| e.to_string())?;
                Ok(conn_db.last_insert_rowid())
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let id: i32 = c
                    .query_one(
                        "INSERT INTO connections (name, driver_type, enabled, poll_interval_ms, config) VALUES ($1, $2, $3, $4, $5) RETURNING id;",
                        &[&conn.name, &conn.driver_type, &conn.enabled, &(conn.poll_interval_ms as i32), &pg_types::Json(&conn.config)],
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    .get(0);
                Ok(id as i64)
            }
        }
    }

    async fn update_connection(&self, conn: &ConnModel) -> DbResult<()> {
        match self.be()? {
            Backend::Sqlite(lock) => {
                let cfg_json = serde_json::to_string(&conn.config).map_err(|e| e.to_string())?;
                let c = lock.lock().await;
                c.execute(
                    "UPDATE connections SET name = ?1, driver_type = ?2, enabled = ?3, poll_interval_ms = ?4, config = ?5 WHERE id = ?6;",
                    params![conn.name, conn.driver_type, conn.enabled as i64, conn.poll_interval_ms, cfg_json, conn.id],
                )
                .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute(
                    "UPDATE connections SET name = $1, driver_type = $2, enabled = $3, poll_interval_ms = $4, config = $5 WHERE id = $6;",
                    &[&conn.name, &conn.driver_type, &conn.enabled, &(conn.poll_interval_ms as i32), &pg_types::Json(&conn.config), &(conn.id as i32)],
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn delete_connection(&self, conn_id: i64) -> DbResult<()> {
        // Удаляем теги и их историю явно, не полагаясь на каскад внешних
        // ключей: в старых базах SQLite могли остаться «сироты».
        let ids = {
            let tags = self.get_tags_by_connection(conn_id).await?;
            tags.iter().map(|t| t.id).collect::<Vec<_>>()
        };
        if !ids.is_empty() {
            self.delete_tags(&ids).await?;
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.execute("DELETE FROM tag_groups WHERE connection_id = ?1;", params![conn_id])
                    .map_err(|e| e.to_string())?;
                conn.execute("DELETE FROM connections WHERE id = ?1;", params![conn_id])
                    .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute("DELETE FROM tag_groups WHERE connection_id = $1;", &[&(conn_id as i32)])
                    .await
                    .map_err(|e| e.to_string())?;
                c.execute("DELETE FROM connections WHERE id = $1;", &[&(conn_id as i32)])
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn get_all_tags(&self) -> DbResult<Vec<Tag>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        let sql = format!("SELECT {TAG_SELECT} FROM tags ORDER BY id ASC;");
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt.query_map([], tag_from_irow).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let rows = c.query(&sql[..], &[]).await.map_err(|e| e.to_string())?;
                Ok(rows.iter().map(tag_from_prow).collect())
            }
        }
    }

    async fn get_configured_tags(&self) -> DbResult<Vec<Tag>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        // Теги только тех подключений, которые ещё есть в конфигурации:
        // защищает вкладки графиков от «сирот» в старых базах.
        let sql = format!("SELECT {TAG_SELECT_T} FROM tags t INNER JOIN connections c ON c.id = t.connection_id ORDER BY t.id ASC;");
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt.query_map([], tag_from_irow).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let rows = c.query(&sql[..], &[]).await.map_err(|e| e.to_string())?;
                Ok(rows.iter().map(tag_from_prow).collect())
            }
        }
    }

    async fn get_tags_by_connection(&self, conn_id: i64) -> DbResult<Vec<Tag>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let sql = format!("SELECT {TAG_SELECT} FROM tags WHERE connection_id = ?1 ORDER BY id ASC;");
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt.query_map(params![conn_id], tag_from_irow).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let sql = format!("SELECT {TAG_SELECT} FROM tags WHERE connection_id = $1 ORDER BY id ASC;");
                let rows = c.query(&sql[..], &[&(conn_id as i32)]).await.map_err(|e| e.to_string())?;
                Ok(rows.iter().map(tag_from_prow).collect())
            }
        }
    }

    async fn add_tag(&self, tag: &Tag) -> DbResult<i64> {
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.execute(
                    "INSERT INTO tags (connection_id, name, address_str, data_type, scale, offset_val, unit, group_name, log_interval_ms, deadband)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10);",
                    params![
                        tag.connection_id, tag.name, tag.address_str, tag.data_type,
                        tag.scale, tag.offset_val, tag.unit, tag.group_name,
                        tag.log_interval_ms, tag.deadband
                    ],
                )
                .map_err(|e| e.to_string())?;
                conn.execute(
                    "INSERT OR IGNORE INTO tag_groups (connection_id, name) VALUES (?1, ?2);",
                    params![tag.connection_id, tag.group_name],
                )
                .map_err(|e| e.to_string())?;
                Ok(conn.last_insert_rowid())
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let id: i32 = c
                    .query_one(
                        "INSERT INTO tags (connection_id, name, address_str, data_type, scale, offset_val, unit, group_name, log_interval_ms, deadband)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id;",
                        &[
                            &(tag.connection_id as i32), &tag.name, &tag.address_str, &tag.data_type,
                            &tag.scale, &tag.offset_val, &tag.unit, &tag.group_name,
                            &(tag.log_interval_ms as i32), &tag.deadband,
                        ],
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    .get(0);
                c.execute(
                    "INSERT INTO tag_groups (connection_id, name) VALUES ($1, $2) ON CONFLICT (connection_id, name) DO NOTHING;",
                    &[&(tag.connection_id as i32), &tag.group_name],
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(id as i64)
            }
        }
    }

    async fn update_tag(&self, tag: &Tag) -> DbResult<()> {
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.execute(
                    "UPDATE tags SET name = ?1, address_str = ?2, data_type = ?3, scale = ?4, offset_val = ?5, unit = ?6, group_name = ?7, log_interval_ms = ?8, deadband = ?9 WHERE id = ?10;",
                    params![
                        tag.name, tag.address_str, tag.data_type, tag.scale, tag.offset_val,
                        tag.unit, tag.group_name, tag.log_interval_ms, tag.deadband, tag.id
                    ],
                )
                .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute(
                    "UPDATE tags SET name = $1, address_str = $2, data_type = $3, scale = $4, offset_val = $5, unit = $6, group_name = $7, log_interval_ms = $8, deadband = $9 WHERE id = $10;",
                    &[
                        &tag.name, &tag.address_str, &tag.data_type, &tag.scale, &tag.offset_val,
                        &tag.unit, &tag.group_name, &(tag.log_interval_ms as i32), &tag.deadband,
                        &(tag.id as i32),
                    ],
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Пакетное удаление переменных вместе с их историей.
    async fn delete_tags(&self, tag_ids: &[i64]) -> DbResult<()> {
        // Убираем дубликаты.
        let mut ids: Vec<i64> = Vec::new();
        for id in tag_ids {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if ids.is_empty() {
            return Ok(());
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                for chunk in chunks(&ids, 500) {
                    let marks = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                    let mut stmt = conn
                        .prepare_cached(&format!("DELETE FROM data_points WHERE tag_id IN ({marks});"))
                        .map_err(|e| e.to_string())?;
                    let pv: Vec<&dyn rusqlite::types::ToSql> =
                        chunk.iter().map(|i| i as &dyn rusqlite::types::ToSql).collect();
                    stmt.execute(rusqlite::params_from_iter(pv.iter())).map_err(|e| e.to_string())?;
                    drop(stmt);
                    let mut stmt = conn
                        .prepare_cached(&format!("DELETE FROM tags WHERE id IN ({marks});"))
                        .map_err(|e| e.to_string())?;
                    stmt.execute(rusqlite::params_from_iter(pv.iter())).map_err(|e| e.to_string())?;
                }
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                for chunk in chunks(&ids, 500) {
                    let marks = chunk
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("${}", i + 1))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let int_ids: Vec<i32> = chunk.iter().map(|i| *i as i32).collect();
                    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        int_ids.iter().map(|i| i as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
                    c.execute(
                        &format!("DELETE FROM data_points WHERE tag_id IN ({marks});"),
                        &refs[..],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    c.execute(&format!("DELETE FROM tags WHERE id IN ({marks});"), &refs[..])
                        .await
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Пакетный перенос тегов в другую группу/подключение (drag&drop дерева I/O).
    async fn move_tags(&self, tag_ids: &[i64], target_conn_id: i64, target_group: &str) -> DbResult<()> {
        let mut ids: Vec<i64> = Vec::new();
        for id in tag_ids {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if ids.is_empty() {
            return Ok(());
        }
        let group = if target_group.is_empty() { "Общие" } else { target_group };
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                for chunk in chunks(&ids, 500) {
                    let marks = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                    let mut stmt = conn
                        .prepare_cached(&format!(
                            "UPDATE tags SET group_name = ?1, connection_id = ?2 WHERE id IN ({marks});"
                        ))
                        .map_err(|e| e.to_string())?;
                    let mut pv: Vec<&dyn rusqlite::types::ToSql> = vec![&group, &target_conn_id];
                    pv.extend(chunk.iter().map(|i| i as &dyn rusqlite::types::ToSql));
                    stmt.execute(rusqlite::params_from_iter(pv.iter())).map_err(|e| e.to_string())?;
                }
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                for chunk in chunks(&ids, 500) {
                    let marks = (0..chunk.len()).map(|i| format!("${}", i + 3)).collect::<Vec<_>>().join(", ");
                    let target = target_conn_id as i32;
                    let int_ids: Vec<i32> = chunk.iter().map(|i| *i as i32).collect();
                    let mut refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        vec![&group, &target];
                    refs.extend(int_ids.iter().map(|i| i as &(dyn tokio_postgres::types::ToSql + Sync)));
                    c.execute(
                        &format!("UPDATE tags SET group_name = $1, connection_id = $2 WHERE id IN ({marks});"),
                        &refs[..],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        // Группа должна быть объявлена у целевого модуля, чтобы дерево показывало её
        // даже после переноса последнего тега из неё в другую.
        self.add_group(target_conn_id, group).await?;
        Ok(())
    }

    async fn get_groups_by_connection(&self, conn_id: i64) -> DbResult<Vec<String>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        let sql = "SELECT name FROM tag_groups WHERE connection_id = ? ORDER BY name ASC;";
        let psql = "SELECT name FROM tag_groups WHERE connection_id = $1 ORDER BY name ASC;";
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
                let rows = stmt.query_map(params![conn_id], |r| r.get::<_, String>(0)).map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let rows = c.query(psql, &[&(conn_id as i32)]).await.map_err(|e| e.to_string())?;
                Ok(rows.iter().map(|r| r.get(0)).collect())
            }
        }
    }

    async fn add_group(&self, conn_id: i64, name: &str) -> DbResult<()> {
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.execute(
                    "INSERT OR IGNORE INTO tag_groups (connection_id, name) VALUES (?1, ?2);",
                    params![conn_id, name],
                )
                .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute(
                    "INSERT INTO tag_groups (connection_id, name) VALUES ($1, $2) ON CONFLICT (connection_id, name) DO NOTHING;",
                    &[&(conn_id as i32), &name],
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn rename_group(&self, conn_id: i64, old_name: &str, new_name: &str) -> DbResult<()> {
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.execute(
                    "UPDATE tag_groups SET name = ?1 WHERE connection_id = ?2 AND name = ?3;",
                    params![new_name, conn_id, old_name],
                )
                .map_err(|e| e.to_string())?;
                conn.execute(
                    "UPDATE tags SET group_name = ?1 WHERE connection_id = ?2 AND group_name = ?3;",
                    params![new_name, conn_id, old_name],
                )
                .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute(
                    "UPDATE tag_groups SET name = $1 WHERE connection_id = $2 AND name = $3;",
                    &[&new_name, &(conn_id as i32), &old_name],
                )
                .await
                .map_err(|e| e.to_string())?;
                c.execute(
                    "UPDATE tags SET group_name = $1 WHERE connection_id = $2 AND group_name = $3;",
                    &[&new_name, &(conn_id as i32), &old_name],
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Удаляет группу. with_tags=true — вместе с её переменными и историей;
    /// иначе переменные перекладываются в «Общие».
    async fn delete_group(&self, conn_id: i64, name: &str, with_tags: bool) -> DbResult<()> {
        if with_tags {
            let tags = self.get_tags_by_connection(conn_id).await?;
            let ids: Vec<i64> = tags
                .iter()
                .filter(|t| {
                    let g = if t.group_name.is_empty() { "Общие" } else { &t.group_name };
                    g == name
                })
                .map(|t| t.id)
                .collect();
            if !ids.is_empty() {
                self.delete_tags(&ids).await?;
            }
        } else {
            match self.be()? {
                Backend::Sqlite(lock) => {
                    let conn = lock.lock().await;
                    conn.execute(
                        "UPDATE tags SET group_name = 'Общие' WHERE connection_id = ?1 AND group_name = ?2;",
                        params![conn_id, name],
                    )
                    .map_err(|e| e.to_string())?;
                }
                Backend::Postgres(lock) => {
                    let c = lock.lock().await;
                    c.execute(
                        "UPDATE tags SET group_name = 'Общие' WHERE connection_id = $1 AND group_name = $2;",
                        &[&(conn_id as i32), &name],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.execute(
                    "DELETE FROM tag_groups WHERE connection_id = ?1 AND name = ?2;",
                    params![conn_id, name],
                )
                .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute(
                    "DELETE FROM tag_groups WHERE connection_id = $1 AND name = $2;",
                    &[&(conn_id as i32), &name],
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// (группы, счётчики) для дерева за один SQL-запрос:
    /// GROUP BY по tags + declared-группы из tag_groups без тегов.
    async fn get_group_counts(&self, conn_id: i64) -> DbResult<Vec<(String, usize)>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        let mut counts: Vec<(String, usize)> = Vec::new();
        let mut push = |name: String, n: usize| {
            if let Some(item) = counts.iter_mut().find(|c| c.0 == name) {
                item.1 = n;
            } else {
                counts.push((name, n));
            }
        };
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT COALESCE(group_name, 'Общие') AS g, COUNT(*) AS n FROM tags WHERE connection_id = ?1 GROUP BY COALESCE(group_name, 'Общие');",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![conn_id], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)? as usize)))
                    .map_err(|e| e.to_string())?;
                for r in rows {
                    let (g, n) = r.map_err(|e| e.to_string())?;
                    push(g.unwrap_or_else(|| "Общие".into()), n);
                }
                drop(stmt);
                let mut stmt = conn
                    .prepare_cached("SELECT name FROM tag_groups WHERE connection_id = ?1;")
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![conn_id], |r| r.get::<_, Option<String>>(0))
                    .map_err(|e| e.to_string())?;
                for r in rows {
                    if let Some(name) = r.map_err(|e| e.to_string())? {
                        if !counts.iter().any(|c| c.0 == name) {
                            counts.push((name, 0));
                        }
                    }
                }
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let rows = c
                    .query(
                        "SELECT COALESCE(group_name, 'Общие') AS g, COUNT(*) AS n FROM tags WHERE connection_id = $1 GROUP BY COALESCE(group_name, 'Общие');",
                        &[&(conn_id as i32)],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                for r in rows {
                    push(r.get::<_, String>(0), r.get::<_, i64>(1) as usize);
                }
                let rows = c
                    .query("SELECT name FROM tag_groups WHERE connection_id = $1;", &[&(conn_id as i32)])
                    .await
                    .map_err(|e| e.to_string())?;
                for r in rows {
                    let name: String = r.get(0);
                    if !counts.iter().any(|c| c.0 == name) {
                        counts.push((name, 0));
                    }
                }
            }
        }
        Ok(counts)
    }

    async fn get_data_points_range(
        &self,
        tag_id: i64,
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> DbResult<Vec<DataPoint>> {
        if !self.is_available {
            return Ok(Vec::new());
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let sql = "SELECT timestamp, value FROM data_points WHERE tag_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 ORDER BY timestamp ASC;";
                let mut stmt = conn.prepare_cached(sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![tag_id, dt_str(&start), dt_str(&end)], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
                    })
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    let (ts, value) = r.map_err(|e| e.to_string())?;
                    if let Some(dt) = parse_sqlite_ts(&ts) {
                        out.push(DataPoint { timestamp: dt.naive_local(), value });
                    }
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let sql = "SELECT \"timestamp\", value FROM data_points WHERE tag_id = $1 AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 ORDER BY \"timestamp\" ASC;";
                let rows = c
                    .query(
                        sql,
                        &[
                            &(tag_id as i32),
                            &start.with_timezone(&Utc),
                            &end.with_timezone(&Utc),
                        ],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(rows
                    .iter()
                    .map(|r| {
                        let ts: DateTime<Utc> = r.get(0);
                        DataPoint { timestamp: ts.with_timezone(&Local).naive_local(), value: r.get(1) }
                    })
                    .collect())
            }
        }
    }

    /// Точки за последние delta_ms относительно последней записи тега.
    async fn get_data_points_by_span(
        &self,
        tag_id: i64,
        delta_ms: i64,
    ) -> DbResult<(Vec<DataPoint>, f64, f64)> {
        let now = Local::now();
        if !self.is_available {
            return Ok((Vec::new(), (now - chrono::Duration::milliseconds(delta_ms)).timestamp() as f64, now.timestamp() as f64));
        }
        let max_dt: Option<DateTime<Local>> = match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let mut stmt = conn
                    .prepare_cached("SELECT MAX(timestamp) FROM data_points WHERE tag_id = ?1;")
                    .map_err(|e| e.to_string())?;
                let s: Option<String> = stmt
                    .query_row(params![tag_id], |r| r.get::<_, Option<String>>(0))
                    .unwrap_or(None);
                s.as_deref().and_then(parse_sqlite_ts)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let v: Option<DateTime<Utc>> = c
                    .query(
                        "SELECT MAX(\"timestamp\") FROM data_points WHERE tag_id = $1;",
                        &[&(tag_id as i32)],
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    .first()
                    .and_then(|r| r.get(0));
                v.map(|t| t.with_timezone(&Local))
            }
        };
        let Some(max_dt) = max_dt else {
            let start = now - chrono::Duration::milliseconds(delta_ms);
            return Ok((Vec::new(), start.timestamp() as f64, now.timestamp() as f64));
        };
        let start_dt = max_dt - chrono::Duration::milliseconds(delta_ms);
        let points = self.get_data_points_range(tag_id, start_dt, max_dt).await?;
        Ok((points, start_dt.timestamp() as f64, max_dt.timestamp() as f64))
    }
}
