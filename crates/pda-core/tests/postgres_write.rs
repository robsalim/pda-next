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

use pda_core::database::db_service::{
    DatabaseService, DbConfig, DbConfigStore, DbWriter, MAX_POINTS,
};
use pda_core::models::{Connection, Engine};

/// Подготовка базы сериализуем: тесты стартуют параллельно в одном процессе,
/// а CREATE DATABASE и ensure_postgres_schema (CREATE ... IF NOT EXISTS)
/// под конкурентным доступом не идемпотентны — гонка даёт «db error».
static PG_INIT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// purge_before чистит таблицу целиком, поэтому тесты, которые пишут и потом
/// читают историю, не должны пересекаться во времени.
static DATA_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn pg_cfg() -> DbConfig {
    DbConfig {
        engine: Engine::Postgres,
        host: std::env::var("PDA_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("PDA_PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(5432),
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
    if tokio::net::TcpStream::connect((cfg.host.as_str(), cfg.port))
        .await
        .is_err()
    {
        eprintln!(
            "SKIP postgres_write: PostgreSQL {}:{} недоступен",
            cfg.host, cfg.port
        );
        return None;
    }
    let _guard = PG_INIT.lock().await;
    DatabaseService::create_postgres_database(&cfg)
        .await
        .unwrap_or_else(|e| panic!("создание тестовой базы '{}': {e}", cfg.dbname));
    let db = DatabaseService::open(cfg).await;
    assert!(
        db.is_available,
        "тестовая база не открылась: {:?}",
        db.last_error
    );
    Some(db)
}

/// Регрессия на `error serializing parameter N`: пачечный INSERT в
/// data_points (TIMESTAMPTZ, INT4, DOUBLE PRECISION, SMALLINT) обязан
/// проходить типизацию tokio-postgres, а точки — возвращаться roundtrip.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_data_points_roundtrip() {
    let Some(db) = open_test_db().await else {
        return;
    };
    let _guard = DATA_LOCK.lock().await;

    // Уникальный tag_id на запуск: тест можно повторять без ручной чистки.
    let tag_id = 900_000i64 + (Local::now().timestamp() % 100_000);
    let now = Local::now();
    let records: Vec<_> = (0..3i64)
        .map(|i| (now + Duration::seconds(i), tag_id, 1.5 + i as f64, 0i32))
        .collect();

    let n = db
        .insert_batch(&records)
        .await
        .expect("insert_batch: пачка должна записаться");
    assert_eq!(n, records.len());

    let got = db
        .get_data_points_range(
            tag_id,
            now - Duration::minutes(1),
            now + Duration::minutes(1),
        )
        .await
        .expect("чтение диапазона");
    assert_eq!(got.len(), 3, "все три точки должны вернуться");
    for (i, p) in got.iter().enumerate() {
        assert!(
            (p.value - (1.5 + i as f64)).abs() < 1e-9,
            "значение точки {i}: {}",
            p.value
        );
    }

    // Убираем за собой: в тестовой базе других данных нет.
    let removed = db
        .purge_before(now + Duration::minutes(5))
        .await
        .expect("purge");
    assert!(removed >= 3, "purge должен удалить вставленное: {removed}");
}

/// Регрессия на сериализацию jsonb (фикс v1.0.0): add_connection с
/// config-объектом, переименование через update_connection, чтение и удаление.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_connection_jsonb_roundtrip() {
    let Some(db) = open_test_db().await else {
        return;
    };

    let mut config = HashMap::new();
    config.insert("ip".to_string(), serde_json::json!("127.0.0.1"));
    config.insert("port".to_string(), serde_json::json!(15502));

    let conn = Connection::new(0, "PG TEST", "modbus_client", true, 500, config);
    let id = db
        .add_connection(&conn)
        .await
        .expect("add_connection с jsonb config");
    assert!(id > 0);

    let mut renamed = conn.clone();
    renamed.id = id;
    renamed.name = "PG TEST renamed".into();
    db.update_connection(&renamed)
        .await
        .expect("update_connection (rename)");

    let all = db.get_all_connections().await.unwrap();
    let found = all
        .iter()
        .find(|c| c.id == id)
        .expect("подключение живо после rename");
    assert_eq!(found.name, "PG TEST renamed");
    assert_eq!(
        found.config.get("port").and_then(|v| v.as_i64()),
        Some(15502),
        "jsonb roundtrip"
    );

    db.delete_connection(id).await.expect("delete_connection");
    assert!(
        db.get_all_connections()
            .await
            .unwrap()
            .iter()
            .all(|c| c.id != id),
        "удалено"
    );
}

/// Прореживание больших выборок (screen-space bucketing).
///
/// Проверяет два свойства, без которых графики на большой базе непригодны:
///  1. ответ не растёт вместе с числом строк в окне (иначе «открыл график —
///     сервер висит» на десятках миллионов точек);
///  2. все теги одного окна возвращают ОДИНАКОВЫЕ метки времени. uPlot строит
///     серии по общей оси X, поэтому у тега с более редкой историей значения
///     иначе уезжают вправо и график показывает ложную картину.
///
/// Для корректной проверки используется batch-метод get_series_window, который
/// принимает решение о прореживании ОДИН РАЗ на окно, а не на тег.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_downsample_dense_grid() {
    let Some(db) = open_test_db().await else {
        return;
    };
    let _guard = DATA_LOCK.lock().await;

    let tag_dense = 910_000i64 + (Local::now().timestamp() % 100_000);
    let tag_sparse = tag_dense + 1;
    let now = Local::now();
    let n = MAX_POINTS as i64 + 500;

    // Плотный тег: каждый миллисекунду. Разреженный: каждый второй.
    // Оба превышают MAX_POINTS — batch-метод включит bucketing и даст
    // одинаковую сетку.
    let mut records = Vec::with_capacity(n as usize * 2);
    for i in 0..n {
        records.push((now + Duration::milliseconds(i), tag_dense, i as f64, 0i32));
    }
    for i in (0..n).step_by(2) {
        records.push((
            now + Duration::milliseconds(i),
            tag_sparse,
            (i * 2) as f64,
            0i32,
        ));
    }
    let written = db
        .insert_batch(&records)
        .await
        .expect("пачка должна записаться");
    assert_eq!(written, records.len());

    let start = now - Duration::seconds(1);
    let end = now + Duration::milliseconds(n) + Duration::seconds(1);

    let series = db
        .get_series_window(&[tag_dense, tag_sparse], start, end)
        .await
        .expect("batch-чтение");

    assert_eq!(series.len(), 2, "оба тега должны быть в ответе");

    // Найдём серии по тегам (порядок не гарантирован).
    let dense = series
        .iter()
        .find(|(tid, _)| *tid == tag_dense)
        .map(|(_, pts)| pts.as_slice())
        .expect("плотный тег должен быть в ответе");
    let sparse = series
        .iter()
        .find(|(tid, _)| *tid == tag_sparse)
        .map(|(_, pts)| pts.as_slice())
        .expect("разреженный тег должен быть в ответе");

    assert!(
        dense.len() <= MAX_POINTS + 1,
        "выборка должна быть прорежена: {} > {}",
        dense.len(),
        MAX_POINTS
    );
    assert_eq!(
        dense.len(),
        sparse.len(),
        "число точек обязано совпадать: dense={} sparse={}",
        dense.len(),
        sparse.len()
    );
    let t_dense: Vec<_> = dense.iter().map(|p| p.timestamp).collect();
    let t_sparse: Vec<_> = sparse.iter().map(|p| p.timestamp).collect();
    assert_eq!(
        t_dense, t_sparse,
        "метрики времени должны совпадать построчно"
    );

    // Прореживание не должно выдумывать данные: значения лежат в диапазоне.
    assert!(
        dense
            .iter()
            .all(|p| p.value >= -0.001 && p.value < n as f64 + 0.001),
        "значения вне диапазона опроса"
    );
}

/// Регрессия на панику `error deserializing column 0` в ПУТИ ОДИНОЧНОГО тега
/// (`get_data_points_range` — его дёргает /api/points). Batch-тест выше её не
/// ловил: там колонка бакета читалась как i64, а здесь — как f64, хотя SQL
/// возвращает bigint (tokio-postgres строго сериализует тип под колонку).
#[tokio::test(flavor = "multi_thread")]
async fn postgres_downsample_single_tag_range() {
    let Some(db) = open_test_db().await else {
        return;
    };
    let _guard = DATA_LOCK.lock().await;

    let tag = 930_000i64 + (Local::now().timestamp() % 100_000);
    let now = Local::now();
    let n = MAX_POINTS as i64 * 3;
    let records: Vec<_> = (0..n)
        .map(|i| (now + Duration::milliseconds(i), tag, i as f64, 0i32))
        .collect();
    db.insert_batch(&records)
        .await
        .expect("пачка должна записаться");

    let start = now - Duration::seconds(1);
    let end = now + Duration::milliseconds(n) + Duration::seconds(1);
    let points = db
        .get_data_points_range(tag, start, end)
        .await
        .expect("чтение прореженного окна не должно паниковать");

    assert!(
        !points.is_empty() && points.len() <= MAX_POINTS,
        "одиночный путь должен прореживать до сетки бакетов: {} > {}",
        points.len(),
        MAX_POINTS
    );
}

/// Регрессия на summary-путь (окна от ~33 часов, bucket >= 1 минуты).
///
/// Класс бага, который этот тест ловит: колонка avg_value была REAL, а
/// tokio-postgres читал её как f64 — паника десериализатора рвала соединение
/// («Базовое соединение закрыто» на фронте, пустой график на Charts).
/// Остальные тесты остаются в окне до 24 часов и summary не касаются.
#[tokio::test(flavor = "multi_thread")]
async fn postgres_summary_large_window() {
    let Some(db) = open_test_db().await else {
        return;
    };
    let _guard = DATA_LOCK.lock().await;

    let tag = 950_000i64 + (Local::now().timestamp() % 100_000);
    let now = Local::now();
    let span = Duration::days(5);
    // По одной записи в каждый минутный бакет окна: 5*1440 = 7200 записей.
    let n = span.num_seconds() / 60;
    let records: Vec<_> = (0..n)
        .map(|i| (now - span + Duration::minutes(i), tag, i as f64, 0i32))
        .collect();
    db.insert_batch(&records)
        .await
        .expect("пачка на 5 суток должна записаться");

    // Batch-путь: окно 5 суток => bucket = 216_000 мс >= 60_000, summary.
    let series = db
        .get_series_window(&[tag], now - span, now)
        .await
        .expect("окно через summary не должно рвать соединение");
    assert_eq!(series.len(), 1, "запрошенный тег должен вернуться");
    let points = &series[0].1;
    assert!(
        !points.is_empty() && points.len() <= MAX_POINTS + 1,
        "summary-окно обязано прореживаться до сетки: {}",
        points.len()
    );

    // Одиночный путь (/api/points) на том же окне.
    let single = db
        .get_data_points_range(tag, now - span, now)
        .await
        .expect("одиночное summary-окно не должно рвать соединение");
    assert!(
        !single.is_empty() && single.len() <= MAX_POINTS + 1,
        "одиночный summary-путь: {}",
        single.len()
    );
}
