//! SQLite-тесты методов, появившихся для панели архива и статуса БД:
//! get_last_data_time (метка последней записи) и get_db_info (размер + теги).
//!
//! В отличие от postgres_write.rs, внешних сервисов не требуют — файл БД
//! создаётся в temp-каталоге и удаляется после теста.

use chrono::{Duration, Local};
use std::collections::HashMap;

use pda_core::database::db_service::{DatabaseService, DbConfig, DbConfigStore, DbWriter};
use pda_core::models::{Connection, Engine, Tag};

/// Открывает свежую SQLite-базу в temp-каталоге.
async fn open_temp_sqlite(name: &str) -> DatabaseService {
    let dir = std::env::temp_dir().join(format!("pda_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp-каталог");
    let path = dir.join("test.db").to_string_lossy().into_owned();
    let cfg = DbConfig {
        engine: Engine::Sqlite,
        sqlite_path: path,
        ..Default::default()
    };
    let db = DatabaseService::open(cfg).await;
    assert!(db.is_available, "SQLite-база не открылась: {:?}", db.last_error);
    db
}

#[tokio::test(flavor = "multi_thread")]
async fn last_data_time_and_db_info() {
    let db = open_temp_sqlite("last_info").await;

    // Пустая база: последней записи нет, тегов ноль, файл уже создан (схема).
    assert_eq!(db.get_last_data_time().await.unwrap(), None);
    let (size0, tags0) = db.get_db_info().await.unwrap();
    assert_eq!(tags0, 0);
    assert!(size0.unwrap_or(0) > 0, "файл БД со схемой должен иметь размер");

    // Тег через подключение (в SQLite tags.connection_id ссылается на connections).
    let mut config = HashMap::new();
    config.insert("ip".to_string(), serde_json::json!("127.0.0.1"));
    let conn = Connection::new(0, "T", "modbus_client", true, 500, config);
    let conn_id = db.add_connection(&conn).await.expect("add_connection");
    let tag = Tag::new(0, conn_id, "T1", "0", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let tag_id = db.add_tag(&tag).await.expect("add_tag");

    let now = Local::now();
    let records: Vec<_> = (0..3i64)
        .map(|i| (now - Duration::seconds(10 - i), tag_id, i as f64, 0i32))
        .collect();
    db.insert_batch(&records).await.expect("insert_batch");

    // get_last_data_time обязан вернуть метку САМОЙ свежей точки в мс.
    // SQLite хранит секунды (dt_str) — допускаем округление до ±1 с.
    let last = db.get_last_data_time().await.unwrap().expect("есть запись");
    let expect = now.timestamp_millis() - Duration::seconds(7).num_milliseconds();
    assert!(
        (last - expect).abs() <= 1000,
        "метка последней записи: got {last}, expected {expect}"
    );

    // Сводка: один тег, размер вырос вместе с данными.
    let (size, tags) = db.get_db_info().await.unwrap();
    assert_eq!(tags, 1);
    assert!(size.unwrap() >= size0.unwrap(), "размер не должен уменьшаться");
}
