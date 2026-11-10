//! Регрессия: bucketing SQLite обязан считать сетку бакетов по ЛОКАЛЬНОМУ
//! времени, а не по UTC.
//!
//! Почему это вообще можно сломать: timestamp в data_points хранится наивной
//! локальной строкой (dt_str), а julianday() без модификатора трактует её как
//! UTC. На машине с ненулевым часовым поясом (+5 ч на этой) сетка бакетов из
//! SQL уезает на offset, densify_buckets не находит ни одной точки в окне —
//! и все прореженные окна возвращаются ПУСТЫМИ. Именно на этот путь опирается
//! подгрузка графика при зуме (/api/series, /api/points при > MAX_POINTS точек).
//!
//! Тест пишу на SQLite, внешние сервисы не нужны.

use std::collections::HashMap;

use chrono::{Duration, Local, TimeZone};

use pda_core::database::db_service::{
    DatabaseService, DbConfig, DbConfigStore, DbWriter, MAX_POINTS,
};
use pda_core::models::{Connection, Engine, Tag};

/// Формула шага бакета — та же, что в bucket_ms() внутри сервиса.
fn bucket_ms_of(span_ms: i64) -> i64 {
    (span_ms.max(1) + MAX_POINTS as i64 - 1) / MAX_POINTS as i64
}

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
    assert!(
        db.is_available,
        "SQLite-база не открылась: {:?}",
        db.last_error
    );
    db
}

/// Именованный момент локального времени — чтобы окно теста не зависело от
/// того, «сейчас» в базе или нет.
fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<Local> {
    Local
        .with_ymd_and_hms(y, mo, d, h, mi, s)
        .single()
        .or_else(|| Local.with_ymd_and_hms(y, mo, d, h, mi, s).earliest())
        .expect("момент")
}

#[tokio::test(flavor = "multi_thread")]
async fn bucketed_window_beyond_last_data_has_no_tail() {
    let db = open_temp_sqlite("bucket_tail").await;

    let mut config = HashMap::new();
    config.insert("ip".to_string(), serde_json::json!("127.0.0.1"));
    let conn = Connection::new(0, "T", "modbus_client", true, 500, config);
    let conn_id = db.add_connection(&conn).await.expect("add_connection");
    let tag = Tag::new(0, conn_id, "T1", "0", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let tag_id = db.add_tag(&tag).await.expect("add_tag");

    // История: [00:00, 03:00] с шагом 1 с. Окно запроса уходит на час в
    // «будущее» (как догрузочная маржа при зуме out). Старый LOCF дорисовывал
    // ровную линию за последней записью; теперь там NaN (в JSON — null), и
    // uPlot обрывает трассу.
    let start = local(2026, 11, 6, 0, 0, 0);
    let data_end = start + Duration::hours(3);
    let records: Vec<_> = (0..=3 * 3600)
        .map(|i| (start + Duration::seconds(i), tag_id, (i % 100) as f64, 0i32))
        .collect();
    db.insert_batch(&records).await.expect("insert_batch");

    let q_end = data_end + Duration::hours(1);
    let series = db
        .get_series_window(&[tag_id], start, q_end)
        .await
        .expect("get_series_window");
    let pts = &series[0].1;
    assert!(!pts.is_empty());
    // Общая ось X окна обязана остаться плотной сеткой постоянной длины —
    // усекать хвост нельзя, иначе разъедется сетка всех тегов окна.
    assert!(
        pts.last().unwrap().timestamp > data_end.naive_local(),
        "сетка должна покрывать всё окно (обрыв линии делает null-значение)"
    );
    // Хвост за последней записью — NaN.
    let tail_nan = pts
        .iter()
        .filter(|p| p.timestamp > data_end.naive_local())
        .all(|p| p.value.is_nan());
    assert!(tail_nan, "в будущее дорисовано реальное значение, а не null");
    // Начало данных (окно стартует с записей) — значения нормальные.
    assert!(pts[0].value.is_finite(), "первый бакет окна с данными стал null");
    // NaN обязан дойти до JSON как null (points_payload + serde_json).
    let json = serde_json::to_string(&pts.iter().map(|p| p.value).collect::<Vec<_>>())
        .expect("NaN не сериализуется");
    assert!(json.contains("null"), "ожидается null в JSON, получено: {json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn bucketed_windows_are_not_empty_on_nonzero_tz() {
    let db = open_temp_sqlite("bucketing").await;

    let mut config = HashMap::new();
    config.insert("ip".to_string(), serde_json::json!("127.0.0.1"));
    let conn = Connection::new(0, "T", "modbus_client", true, 500, config);
    let conn_id = db.add_connection(&conn).await.expect("add_connection");
    let tag = Tag::new(0, conn_id, "T1", "0", "FLOAT", 1.0, 0.0, "", "Общие", 1000, 0.0);
    let tag_id = db.add_tag(&tag).await.expect("add_tag");

    // Ровно 3 часа dense-истории с шагом 1 секунду: 10801 точка > MAX_POINTS,
    // поэтому и пакетный, и одиночный запросы идут через bucketed-путь.
    let start = local(2026, 11, 6, 0, 0, 0);
    let span_ms = 3 * 3600 * 1000;
    let end = start + Duration::milliseconds(span_ms);
    let n = 3 * 3600 + 1;
    let records: Vec<_> = (0..n)
        .map(|i| {
            (
                start + Duration::seconds(i),
                tag_id,
                (i % 100) as f64,
                0i32,
            )
        })
        .collect();
    db.insert_batch(&records).await.expect("insert_batch");

    // 1) Пакетный путь /api/series: сетка бакетов обязана попасть в окно.
    let series = db
        .get_series_window(&[tag_id], start, end)
        .await
        .expect("get_series_window");
    assert_eq!(series.len(), 1, "один тег — одна серия");
    let pts = &series[0].1;
    assert!(
        !pts.is_empty(),
        "прореженное окно пустое: бакеты SQL не совпали с локальной сеткой (TZ-баг)"
    );
    assert!(
        pts.len() <= MAX_POINTS + 2,
        "прореживание должно дать не больше MAX_POINTS, получено {}",
        pts.len()
    );

    // Шаг сетки — постоянный bucket_ms, первая метка — не позже начала окна
    // (первый бакет может начинаться чуть раньше start за счёт floor).
    let bucket = bucket_ms_of(span_ms);
    let t0 = start.naive_local();
    let t1 = end.naive_local();
    let first = pts[0].timestamp;
    assert!(
        first >= t0 - Duration::milliseconds(bucket) && first <= t0 + Duration::milliseconds(bucket),
        "первая метка бакета {first} далеко от начала окна {t0} (сдвиг сетки?)"
    );
    for w in pts.windows(2) {
        let step = (w[1].timestamp - w[0].timestamp).num_milliseconds();
        assert_eq!(step, bucket, "неравный шаг сетки бакетов");
    }
    assert!(
        pts.last().unwrap().timestamp <= t1,
        "метки вышли за правый край окна"
    );

    // 2) Одиночный путь /api/points (тот же bucketed-SQL в get_data_points_range).
    let (single, _, _) = db
        .get_data_points_by_span(tag_id, span_ms)
        .await
        .expect("get_data_points_by_span");
    assert!(
        !single.is_empty(),
        "одиночное прореженное окно пустое — /api/points сломан тем же багом"
    );

    // 3) Занятые бакеты (подсветка разрывов): метки обязаны лежать в окне.
    let busy = db
        .get_busy_buckets(&[tag_id], start, end, 60_000)
        .await
        .expect("get_busy_buckets");
    assert!(!busy.is_empty(), "нет ни одного занятого бакета при dense-истории");
    let (ms0, ms1) = (start.timestamp_millis(), end.timestamp_millis());
    for b in &busy {
        assert!(
            *b >= ms0 - 60_000 && *b <= ms1 + 60_000,
            "бакет {b} вне окна [{ms0}, {ms1}] — сетка сдвинута на часовой пояс"
        );
    }
}
