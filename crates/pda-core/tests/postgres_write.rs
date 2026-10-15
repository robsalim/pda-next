//! Интеграционные тесты PostgreSQL (реальный сервер).
//!
//! Зачем этот файл: все юнит-тесты и смоук-тест сервера крутятся на SQLite,
//! а rusqlite динамически типизирован — принимает в параметр что угодно.
//! Класс багов «tokio-postgres строго сериализует тип под колонку» был
//! невидим для них полностью: v1.1.0 история молча вставала в ошибку
//! `error serializing parameter 1` (tag_id i64 -> INT4, quality i32 -> INT2),
//! а v1.0.0 — config String -> jsonb. Ошибку было видно только в панели
//! «Журнал», запись в БД при этом не шла вовсе.
//!
//! Правила:
//!  - сервер берётся из окружения PDA_PG_HOST/PORT/USER/PASSWORD/DB
//!    (дефолты — как в DbConfig::default, база pda_next_test);
//!  - используется ОТДЕЛЬНАЯ база pda_next_test — боевые данные не трогаем;
//!  - если PostgreSQL недоступен, тесты пропускаются (не фейл), чтобы
//!    `cargo test` работал на машинах без PG.

use std::collections::HashMap;

use chrono::{Duration, Local};

use pda_core::database::db_service::{DbConfig, DbConfigStore, DbWriter, DatabaseService};
use pda_core::models::{Connection, Engine};

/// Подготовка базы сериализуем: тесты стартуют параллельно в одном процессе,
/// а CREATE DATABASE и ensure_postgres_schema (CREATE ... IF NOT EXISTS)
/// под конкурентным доступом не идемпотентны — гонка даёт «db error».
static PG_INIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn pg_cfg() -> DbConfig {
    DbConfig {
        engine: Engine::Postgres,
        host: std::env::var("PDA_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("PDA_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(5432),
        user: std::env::var("PDA_PG_USER").unwrap_or_else(|_| "postgres".into()),
        password: std::env::var("PDA_PG_PASSWORD").unwrap_or_else(|_| DbConfig::default().password),
        dbname: std::env::var("PDA_PG_DB").unwrap_or_else(|_| "pda_next_test".into()),
        ..Default::default()
    }
}

/// Готовит отдельную тестовую базу и открывает сервис.
/// None — PostgreSQL недоступен, тест должен быть пропущен.
async fn open_test_db() -> Option<DatabaseService> {
    let cfg = pg_cfg();
    if tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port)).await.is_err() {
        eprintln!("SKIP postgres_write: PostgreSQL {}:{} недоступен", cfg.host, cfg.port);
        return None;
    }
    let _guard = PG_INIT.lock().await;
    DatabaseService::create_postgres_database(&cfg)
        .await
        .unwrap_or_else(|e| panic!("создание тестовой базы '{}': {e}", cfg.dbname));
    let db = DatabaseService::open(cfg).await;
    assert!(db.is_available, "тестовая база не открылась: {:?}", db.last_error);
    Some(db)
}

/// Регрессия на `error serializing parameter N`: пачечный INSERT в
/// data_points (TIMESTAMPTZ, INT4, DOUBLE PRECISION, SMALLINT) обязан
/// проходить типизацию tokio-postgres, а точки — возвращаться roundtrip.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_data_points_roundtrip() {
    let Some(db) = open_test_db().await else { return };

    // Уникальный tag_id на запуск: тест можно повторять без ручной чистки.
    let tag_id = 900_000i64 + (Local::now().timestamp() % 100_000);
    let now = Local::now();
    let records: Vec<_> = (0..3i64)
        .map(|i| (now + Duration::seconds(i), tag_id, 1.5 + i as f64, 0i32))
        .collect();

    let n = db.insert_batch(&records).await.expect("insert_batch: пачка должна записаться");
    assert_eq!(n, records.len());

    let got = db
        .get_data_points_range(tag_id, now - Duration::minutes(1), now + Duration::minutes(1))
        .await
        .expect("чтение диапазона");
    assert_eq!(got.len(), 3, "все три точки должны вернуться");
    for (i, p) in got.iter().enumerate() {
        assert!((p.value - (1.5 + i as f64)).abs() < 1e-9, "значение точки {i}: {}", p.value);
    }

    // Убираем за собой: в тестовой базе других данных нет.
    let removed = db.purge_before(now + Duration::minutes(5)).await.expect("purge");
    assert!(removed >= 3, "purge должен удалить вставленное: {removed}");
}

/// Регрессия на сериализацию jsonb (фикс v1.0.0): add_connection с
/// config-объектом, переименование через update_connection, чтение и удаление.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_connection_jsonb_roundtrip() {
    let Some(db) = open_test_db().await else { return };

    let mut config = HashMap::new();
    config.insert("ip".to_string(), serde_json::json!("127.0.0.1"));
    config.insert("port".to_string(), serde_json::json!(15502));

    let conn = Connection::new(0, "PG TEST", "modbus_client", true, 500, config);
    let id = db.add_connection(&conn).await.expect("add_connection с jsonb config");
    assert!(id > 0);

    let mut renamed = conn.clone();
    renamed.id = id;
    renamed.name = "PG TEST renamed".into();
    db.update_connection(&renamed).await.expect("update_connection (rename)");

    let all = db.get_all_connections().await.unwrap();
    let found = all.iter().find(|c| c.id == id).expect("подключение живо после rename");
    assert_eq!(found.name, "PG TEST renamed");
    assert_eq!(found.config.get("port").and_then(|v| v.as_i64()), Some(15502), "jsonb roundtrip");

    db.delete_connection(id).await.expect("delete_connection");
    assert!(db.get_all_connections().await.unwrap().iter().all(|c| c.id != id), "удалено");
}
