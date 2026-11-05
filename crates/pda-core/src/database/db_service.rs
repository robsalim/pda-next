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
use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone, Timelike, Utc};
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
    Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
        .map(|d| d.into())
}

/// Нарезка под лимит количества параметров одного запроса.
fn chunks<T>(items: &[T], size: usize) -> Vec<&[T]> {
    items.chunks(size.max(1)).collect()
}

/// Сырой запрос SQLite: все точки без даунсемплинга (когда их <= MAX_POINTS).
fn query_raw_sqlite(
    conn: &Connection,
    tag_id: i64,
    start: &DateTime<Local>,
    end: &DateTime<Local>,
) -> DbResult<Vec<DataPoint>> {
    let sql = format!(
        "SELECT timestamp, value FROM data_points WHERE tag_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 \
         ORDER BY timestamp ASC LIMIT {limit};",
        limit = MAX_POINTS + 100
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![tag_id, dt_str(start), dt_str(end)], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        let (ts, value) = r.map_err(|e| e.to_string())?;
        if let Some(dt) = parse_sqlite_ts(&ts) {
            out.push(DataPoint {
                timestamp: dt.naive_local(),
                value,
            });
        }
    }
    Ok(out)
}

/// Предел точек на один график. Экран ~1–2 тыс. пикселей по X, поэтому больше
/// рисовать бессмысленно: при десятках миллионов строк в БД такая выборка и
/// такая передача — единственная причина «виснет при открытии графика».
pub const MAX_POINTS: usize = 2000;

/// Granularity of the pre-aggregated summary table (1 minute).
/// For windows larger than this, queries go to data_points_summary instead of
/// data_points, which is ~1000x smaller and answers in <100ms instead of 20s.
const SUMMARY_BUCKET_MS: i64 = 60_000;

/// Шаг бакета прореживания в целых миллисекундах для окна заданной длины.
///
/// Дробный шаг недопустим: SQL считает его точно, а Rust при приведении к
/// i64 потерял бы дробь — сетки меток на сервере и в плотной выборке
/// разошлись бы, и серии на графике поехали бы по оси X.
fn bucket_ms(span_ms: i64) -> i64 {
    let buckets = MAX_POINTS.max(1) as i64;
    (span_ms.max(1) + buckets - 1) / buckets
}

/// Серийный запрос истории для нескольких тегов одного окна.
///
/// Возвращает выровненные серии: все теги получают одинаковую сетку
/// времени (решение о прореживании принимается один раз на окно), иначе
/// uPlot сдвинет значения одного сигнала относительно другого.
///
/// Возвращаемый тип: список пар (tag_id, Vec<DataPoint>).
/// Точки внутри каждой серии упорядочены по времени.
impl DatabaseService {
    /// Пакетное чтение истории для нескольких тегов.
    /// Если суммарно точек в окне больше MAX_POINTS — включается
    /// screen-space bucketing, и все серии получают одинаковую сетку.
    pub async fn get_series_window(
        &self,
        tag_ids: &[i64],
        start: DateTime<Local>,
        end: DateTime<Local>,
    ) -> DbResult<Vec<(i64, Vec<DataPoint>)>> {
        if tag_ids.is_empty() || !self.is_available {
            return Ok(Vec::new());
        }
        let span_ms = (end - start).num_milliseconds();
        let bucket = bucket_ms(span_ms);
        let do_downsample = bucket < span_ms.max(1) && span_ms > 0;

        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                if !do_downsample {
                    return get_series_raw_sqlite(&conn, tag_ids, &start, &end);
                }
                get_series_bucketed_sqlite(&conn, tag_ids, &start, &end, bucket)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                if !do_downsample {
                    return get_series_raw_pg(&c, tag_ids, &start, &end).await;
                }
                // Если шаг прореживания крупнее минутного бакета (окно от
                // ~33 часов), берём готовые средние из data_points_summary —
                // таблица в ~1000x меньше и отвечает быстрее. Окна с шагом
                // бакета мельче минуты summary не обслуживает: в ней просто
                // нет таких бакетов, и densify вернул бы пустоту.
                if bucket >= SUMMARY_BUCKET_MS {
                    let summary = query_summary_pg(&c, tag_ids, &start, &end).await?;
                    let start_ms = start.timestamp_millis();
                    let end_ms = end.timestamp_millis();
                    let mut result = Vec::new();
                    for (tag_id, rows) in summary {
                        let points = densify_buckets(rows, start_ms, end_ms, bucket);
                        result.push((tag_id, points));
                    }
                    return Ok(result);
                }
                get_series_bucketed_pg(&c, tag_ids, &start, &end, bucket, self.timescale).await
            }
        }
    }

    /// Метка времени самой свежей записи в data_points (epoch, мс) — по всей
    /// базе, без фильтра по тегу. Нужна режиму Archive: показывать пользователю,
    /// до какого момента вообще есть история, чтобы не грузить пустой период.
    ///
    /// ORDER BY timestamp DESC LIMIT 1 вместо MAX(timestamp): SQLite и
    /// PostgreSQL/Timescale сводят такой запрос к одной записи индекса
    /// (idx_dp_ts_only), тогда как MAX по гипертаблице агрегирует каждый чанк.
    pub async fn get_last_data_time(&self) -> DbResult<Option<i64>> {
        if !self.is_available {
            return Ok(None);
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                let s: Option<String> = conn
                    .query_row(
                        "SELECT timestamp FROM data_points ORDER BY timestamp DESC LIMIT 1;",
                        [],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .unwrap_or(None);
                Ok(s.as_deref()
                    .and_then(parse_sqlite_ts)
                    .map(|d| d.timestamp_millis()))
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let v: Option<DateTime<Utc>> = c
                    .query(
                        "SELECT \"timestamp\" FROM data_points ORDER BY \"timestamp\" DESC LIMIT 1;",
                        &[],
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    .first()
                    .and_then(|r| r.get(0));
                Ok(v.map(|t| t.timestamp_millis()))
            }
        }
    }

    /// Занятые бакеты окна: моменты (epoch, мс) начала бакета, внутри которого
    /// есть хотя бы одна запись по выбранным тегам.
    ///
    /// Нужен обзору архива, чтобы подсветить интервалы БЕЗ данных: на
    /// прореженном мини-графике «пусто» и «мало сигналов» выглядят одинаково,
    /// а по факту пустой интервал — это остановленный модуль или простой связи.
    /// Отдаётся «сырой» список занятых бакетов, а промежутки между ними
    /// считает фронтенд — так ответ остаётся крошечным при любом окне.
    pub async fn get_busy_buckets(
        &self,
        tag_ids: &[i64],
        start: DateTime<Local>,
        end: DateTime<Local>,
        bucket_ms: i64,
    ) -> DbResult<Vec<i64>> {
        if tag_ids.is_empty() || !self.is_available || bucket_ms <= 0 {
            return Ok(Vec::new());
        }
        match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                busy_buckets_sqlite(&conn, tag_ids, &start, &end, bucket_ms)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                busy_buckets_pg(&c, tag_ids, &start, &end, bucket_ms, self.timescale).await
            }
        }
    }

    /// Сводка для панели статуса: размер базы в байтах и количество тегов.
    /// до чекпойнта живут именно в -wal, их тоже считаем).
    /// PostgreSQL — pg_database_size(current_database()).
    pub async fn get_db_info(&self) -> DbResult<(Option<i64>, usize)> {
        if !self.is_available {
            return Err("База данных недоступна".into());
        }
        let tags: usize = match self.be()? {
            Backend::Sqlite(lock) => {
                let conn = lock.lock().await;
                conn.query_row("SELECT COUNT(*) FROM tags;", [], |r| r.get::<_, i64>(0))
                    .unwrap_or(0) as usize
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.query("SELECT COUNT(*) FROM tags;", &[])
                    .await
                    .map_err(|e| e.to_string())?
                    .first()
                    .and_then(|r| r.get::<_, Option<i64>>(0))
                    .unwrap_or(0) as usize
            }
        };
        let size: Option<i64> = match self.be()? {
            Backend::Sqlite(_) => {
                let p = &self.cfg.sqlite_path;
                let mut total: i64 = 0;
                let mut any = false;
                for suffix in ["", "-wal", "-shm"] {
                    if let Ok(md) = std::fs::metadata(format!("{p}{suffix}")) {
                        total += md.len() as i64;
                        any = true;
                    }
                }
                if any {
                    Some(total)
                } else {
                    None
                }
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.query("SELECT pg_database_size(current_database());", &[])
                    .await
                    .ok()
                    .and_then(|rows| {
                        rows.first().and_then(|r| r.get::<_, Option<i64>>(0))
                    })
            }
        };
        Ok((size, tags))
    }
}

/// Метка времени (мс от эпохи) -> локальный datetime для DataPoint.
fn ts_from_millis(ms: i64) -> NaiveDateTime {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
        .with_timezone(&Local)
        .naive_local()
}

/// Прореженный ответ БД (граница бакета в мс -> среднее) приводит к плотной
/// сетке бакетов с постоянным шагом.
///
/// uPlot строит все серии по единой оси X: если у одного тега бакет оказался
/// пустым, его значения сдвигаются относительно соседнего сигнала. Пустой
/// бакет наследует предыдущее значение (LOCF) — для ступенчатого промышленного
/// сигнала это корректно. Ведущие пустые бакеты отбрасываются.
fn densify_buckets(
    rows: Vec<(f64, f64)>,
    start_ms: i64,
    end_ms: i64,
    bucket_ms: i64,
) -> Vec<DataPoint> {
    if bucket_ms <= 0 {
        return Vec::new();
    }
    let first = start_ms.div_euclid(bucket_ms) * bucket_ms;
    let mut it = rows.into_iter().peekable();
    let mut out = Vec::new();
    let mut last: Option<f64> = None;

    let mut b = first;
    while b <= end_ms {
        while let Some(&(rb, v)) = it.peek() {
            if (rb as i64) <= b {
                last = Some(v);
                it.next();
            } else {
                break;
            }
        }
        if let Some(v) = last {
            out.push(DataPoint {
                timestamp: ts_from_millis(b),
                value: v,
            });
        }
        b += bucket_ms;
    }
    out
}

// ---- Batch helpers: get_series_window (сырой + bucketed) ----

/// Query pre-aggregated summary table for PostgreSQL.
/// Returns (tag_id, Vec<(bucket_ms, avg_value)>).
async fn query_summary_pg(
    c: &tokio_postgres::Client,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
) -> DbResult<Vec<(i64, Vec<(f64, f64)>)>> {
    let p_ids: Vec<i32> = tag_ids.iter().map(|t| *t as i32).collect();
    let p_start = start.with_timezone(&Utc);
    let p_end = end.with_timezone(&Utc);

    let rows = c
        .query(
            "SELECT tag_id, \
             (EXTRACT(EPOCH FROM bucket) * 1000)::bigint, \
             avg_value::float8 \
             FROM data_points_summary \
             WHERE tag_id = ANY($1::int[]) \
               AND bucket >= $2 AND bucket <= $3 \
             ORDER BY tag_id, bucket;",
            &[&p_ids, &p_start, &p_end],
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut by_tag: std::collections::BTreeMap<i64, Vec<(f64, f64)>> =
        std::collections::BTreeMap::new();
    for r in &rows {
        let tag_id: i32 = r.get(0);
        let bucket_ms: i64 = r.get(1);
        let avg: f64 = r.get(2);
        by_tag
            .entry(tag_id as i64)
            .or_default()
            .push((bucket_ms as f64, avg));
    }
    Ok(by_tag.into_iter().collect())
}

/// Сырой запрос для нескольких тегов (SQLite).
fn get_series_raw_sqlite(
    conn: &Connection,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
) -> DbResult<Vec<(i64, Vec<DataPoint>)>> {
    let sql = format!(
        "SELECT tag_id, timestamp, value FROM data_points \
         WHERE tag_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 \
         ORDER BY tag_id, timestamp ASC \
         LIMIT {limit};",
        limit = MAX_POINTS * tag_ids.len().max(1) + 100
    );
    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            params![
                tag_ids.first().cloned().unwrap_or(0),
                dt_str(start),
                dt_str(end)
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            },
        )
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok());

    let mut map: Vec<(i64, Vec<DataPoint>)> = Vec::new();
    let mut cur_tag: Option<i64> = None;
    let mut cur_points: Vec<DataPoint> = Vec::new();

    for (tag_id, ts, value) in rows {
        if cur_tag != Some(tag_id) {
            if let Some(t) = cur_tag {
                map.push((t, std::mem::replace(&mut cur_points, Vec::new())));
            }
            cur_tag = Some(tag_id);
        }
        if let Some(dt) = parse_sqlite_ts(&ts) {
            cur_points.push(DataPoint {
                timestamp: dt.naive_local(),
                value,
            });
        }
    }
    if let Some(t) = cur_tag {
        map.push((t, cur_points));
    }
    Ok(map)
}

/// Сырой запрос для нескольких тегов (PostgreSQL).
async fn get_series_raw_pg(
    c: &tokio_postgres::Client,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
) -> DbResult<Vec<(i64, Vec<DataPoint>)>> {
    let p_ids: Vec<i32> = tag_ids.iter().map(|t| *t as i32).collect();
    let limit = MAX_POINTS * tag_ids.len().max(1) + 100;
    let rows = c
        .query(
            &format!(
                "SELECT tag_id, \"timestamp\", value FROM data_points \
                 WHERE tag_id = ANY($1::int[]) AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 \
                 ORDER BY tag_id, \"timestamp\" ASC \
                 LIMIT {limit};",
            ),
            &[&p_ids, &start.with_timezone(&Utc), &end.with_timezone(&Utc)],
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut map: Vec<(i64, Vec<DataPoint>)> = Vec::new();
    let mut cur_tag: Option<i64> = None;
    let mut cur_points: Vec<DataPoint> = Vec::new();

    for r in &rows {
        let tag_id: i64 = r.get::<_, i32>(0) as i64;
        let ts: DateTime<Utc> = r.get(1);
        let value: f64 = r.get(2);
        if cur_tag != Some(tag_id) {
            if let Some(t) = cur_tag {
                map.push((t, std::mem::replace(&mut cur_points, Vec::new())));
            }
            cur_tag = Some(tag_id);
        }
        cur_points.push(DataPoint {
            timestamp: ts.with_timezone(&Local).naive_local(),
            value,
        });
    }
    if let Some(t) = cur_tag {
        map.push((t, cur_points));
    }
    Ok(map)
}

/// Bucketed aggregate (SQLite).
fn get_series_bucketed_sqlite(
    conn: &Connection,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
    bucket_ms: i64,
) -> DbResult<Vec<(i64, Vec<DataPoint>)>> {
    // SQLite не поддерживает ANY(array) и именованные параметры. Строим
    // динамический IN(?,?,...) и встраиваем bucket_ms как литерал (он вычислен
    // из span, пользовательского ввода тут нет — SQL injection невозможен).
    // CAST AS INTEGER — epoch_ms как целое; / bucket_ms — целочисленное деление.
    // LIMIT — защита от переполнения, если фильтр по времени не сработает.
    let placeholders: String = tag_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let limit = (MAX_POINTS * tag_ids.len().max(1) + 100) as i64;
    let sql = format!(
        "SELECT tag_id, \
         CAST(CAST((julianday(timestamp) - 2440587.5) * 86400000 AS INTEGER) / {b} AS INTEGER) * {b}, \
         AVG(value) \
         FROM data_points \
         WHERE tag_id IN ({ph}) AND timestamp >= ? AND timestamp <= ? \
         GROUP BY 1, 2 ORDER BY 1, 2 \
         LIMIT {lim};",
        b = bucket_ms,
        ph = placeholders,
        lim = limit
    );

    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(tag_ids.len() + 2);
    for id in tag_ids {
        args.push(Box::new(*id));
    }
    args.push(Box::new(dt_str(start)));
    args.push(Box::new(dt_str(end)));

    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.into_iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)? as f64,
                r.get::<_, f64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok());

    // Group by (tag_id, bucket) → collect into dense grid per tag.
    let mut by_tag: std::collections::BTreeMap<i64, Vec<(f64, f64)>> =
        std::collections::BTreeMap::new();
    for (tag_id, bucket_ms_val, avg) in rows {
        by_tag.entry(tag_id).or_default().push((bucket_ms_val, avg));
    }

    let start_ms = start.timestamp_millis();
    let end_ms = end.timestamp_millis();
    let mut result = Vec::new();
    for (tag_id, rows) in by_tag {
        let points = densify_buckets(rows, start_ms, end_ms, bucket_ms);
        result.push((tag_id, points));
    }
    Ok(result)
}

/// Bucketed aggregate (PostgreSQL).
async fn get_series_bucketed_pg(
    c: &tokio_postgres::Client,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
    bucket_ms: i64,
    timescale: bool,
) -> DbResult<Vec<(i64, Vec<DataPoint>)>> {
    let p_ids: Vec<i32> = tag_ids.iter().map(|t| *t as i32).collect();
    let p_start = start.with_timezone(&Utc);
    let p_end = end.with_timezone(&Utc);

    let rows = if timescale {
        // TimescaleDB: используем time_bucket(interval).
        // bucket_ms → интервал литералом в SQL: параметром его не передать —
        // сервер выводит тип $4 как interval, а tokio-postgres сериализует
        // только Rust-типы, у которых есть pg-код interval (String/text — нет).
        // Значение целое и вычислено у нас, инъекция исключена.
        c.query(
            &format!(
                "SELECT tag_id, \
                 (EXTRACT(EPOCH FROM time_bucket('{bucket_ms} milliseconds'::interval, \"timestamp\")) * 1000)::bigint AS b, \
                 AVG(value) \
                 FROM data_points \
                 WHERE tag_id = ANY($1::int[]) AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 \
                 GROUP BY 1, 2 ORDER BY 1, 2;",
            ),
            &[&p_ids, &p_start, &p_end],
        )
        .await
        .map_err(|e| e.to_string())?
    } else {
        // Обычный PostgreSQL: целочисленная арифметика epoch_ms.
        // EXTRACT(EPOCH ...) → float8, *1000 → float8, ::bigint → целое.
        // Деление bigint/bigint → целочисленное, *bigint → bigint.
        let bucket = bucket_ms as i64;
        c.query(
            "SELECT tag_id, \
             (EXTRACT(EPOCH FROM \"timestamp\") * 1000)::bigint / $4::bigint * $4::bigint AS b, \
             AVG(value) \
             FROM data_points \
             WHERE tag_id = ANY($1::int[]) AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 \
             GROUP BY 1, 2 ORDER BY 1, 2;",
            &[&p_ids, &p_start, &p_end, &bucket],
        )
        .await
        .map_err(|e| e.to_string())?
    };

    let mut by_tag: std::collections::BTreeMap<i64, Vec<(f64, f64)>> =
        std::collections::BTreeMap::new();
    for r in &rows {
        let tag_id: i32 = r.get(0);
        let bucket_val: i64 = r.get(1); // bigint
        let avg: f64 = r.get(2);
        by_tag
            .entry(tag_id as i64)
            .or_default()
            .push((bucket_val as f64, avg));
    }

    let start_ms = start.timestamp_millis();
    let end_ms = end.timestamp_millis();
    let mut result = Vec::new();
    for (tag_id, rows) in by_tag {
        let points = densify_buckets(rows, start_ms, end_ms, bucket_ms);
        result.push((tag_id, points));
    }
    Ok(result)
}

// ------------------------------------------------------------------ gaps
// «Занятые» бакеты окна — по ним фронт подсвечивает интервалы без данных.

/// DISTINCT по бакету (SQLite). Шаг бакета — литерал: пользовательского ввода
/// здесь нет, значение вычислено из длины окна.
fn busy_buckets_sqlite(
    conn: &Connection,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
    bucket_ms: i64,
) -> DbResult<Vec<i64>> {
    let placeholders: String = tag_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT DISTINCT CAST(CAST((julianday(timestamp) - 2440587.5) * 86400000 AS INTEGER) / {b} AS INTEGER) * {b} AS b \
         FROM data_points \
         WHERE tag_id IN ({ph}) AND timestamp >= ? AND timestamp <= ? \
         ORDER BY 1 \
         LIMIT {lim};",
        b = bucket_ms,
        ph = placeholders,
        lim = (MAX_POINTS * 2) as i64
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(tag_ids.len() + 2);
    for id in tag_ids {
        args.push(Box::new(*id));
    }
    args.push(Box::new(dt_str(start)));
    args.push(Box::new(dt_str(end)));

    let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.into_iter()), |r| r.get::<_, i64>(0))
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// DISTINCT по бакету (PostgreSQL/Timescale). Для Timescale берём time_bucket,
/// чтобы запрос шёл по индексу чанков, а не сканировал гипертаблицу.
async fn busy_buckets_pg(
    c: &tokio_postgres::Client,
    tag_ids: &[i64],
    start: &DateTime<Local>,
    end: &DateTime<Local>,
    bucket_ms: i64,
    timescale: bool,
) -> DbResult<Vec<i64>> {
    let p_ids: Vec<i32> = tag_ids.iter().map(|t| *t as i32).collect();
    let p_start = start.with_timezone(&Utc);
    let p_end = end.with_timezone(&Utc);
    let lim = (MAX_POINTS * 2) as i64;

    let rows = if timescale {
        c.query(
            &format!(
                "SELECT DISTINCT (EXTRACT(EPOCH FROM time_bucket('{bucket_ms} milliseconds'::interval, \"timestamp\")) * 1000)::bigint AS b \
                 FROM data_points \
                 WHERE tag_id = ANY($1::int[]) AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 \
                 ORDER BY 1 LIMIT {lim};",
            ),
            &[&p_ids, &p_start, &p_end],
        )
        .await
        .map_err(|e| e.to_string())?
    } else {
        c.query(
            "SELECT DISTINCT (EXTRACT(EPOCH FROM \"timestamp\") * 1000)::bigint / $4::bigint * $4::bigint AS b \
             FROM data_points \
             WHERE tag_id = ANY($1::int[]) AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 \
             ORDER BY 1 LIMIT $5;",
            &[&p_ids, &p_start, &p_end, &bucket_ms, &lim],
        )
        .await
        .map_err(|e| e.to_string())?
    };

    Ok(rows.iter().filter_map(|r| r.get::<_, Option<i64>>(0)).collect())
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
    async fn move_tags(
        &self,
        tag_ids: &[i64],
        target_conn_id: i64,
        target_group: &str,
    ) -> DbResult<()>;
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
    /// TimescaleDB hypertable создан — bucketing можно считать через time_bucket.
    pub timescale: bool,
}

impl DatabaseService {
    /// Открытие БД с созданием схемы.
    pub async fn open(cfg: DbConfig) -> Self {
        match cfg.engine {
            Engine::Sqlite => Self::open_sqlite(cfg).await,
            Engine::Postgres => Self::open_postgres(cfg).await,
            Engine::Mysql | Engine::Mariadb => Self::unavailable(
                cfg,
                "MySQL/MariaDB в Rust-версии ещё не перенесён (используйте SQLite или PostgreSQL)"
                    .into(),
            ),
        }
    }

    fn unavailable(cfg: DbConfig, err: DbError) -> Self {
        Self {
            cfg,
            backend: None,
            is_available: false,
            last_error: Some(err),
            timescale: false,
        }
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
            .query(
                "SELECT 1 FROM pg_database WHERE datname = $1",
                &[&cfg.dbname],
            )
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
        drop(client);

        // Не оставляем базу пустой: тут же поднимаем схему и hypertable+сжатие.
        // Иначе «создать базу» даёт нерабочую заготовку, а настройка Timescale
        // откладывается до первого переключения. Ошибку считаем unsuccessful:
        // база создана, но не готова.
        let target = Self::open_postgres(cfg.clone()).await;
        match &target.last_error {
            Some(err) => Err(format!("база создана, но инициализация не удалась: {err}")),
            None => Ok(true),
        }
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
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
            )
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
                timescale: false,
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
                let has_timescale = try_create_timescale(&client).await;
                Self {
                    cfg,
                    backend: Some(Backend::Postgres(client)),
                    is_available: true,
                    last_error: None,
                    timescale: has_timescale,
                }
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
        self.backend
            .clone()
            .ok_or_else(|| "База данных недоступна".to_string())
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
            log_interval_ms INTEGER DEFAULT 0,
            deadband REAL DEFAULT 0.0
        );
        CREATE TABLE IF NOT EXISTS data_points (
            timestamp TEXT NOT NULL,
            tag_id INTEGER NOT NULL,
            value REAL NOT NULL,
            quality INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_dp_ts ON data_points(tag_id, timestamp DESC);
        CREATE INDEX IF NOT EXISTS idx_dp_ts_only ON data_points(timestamp DESC);
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
        conn.execute(
            "ALTER TABLE tags ADD COLUMN log_interval_ms INTEGER DEFAULT 0;",
            [],
        )?;
    }
    if !has_col("tags", "deadband")? {
        conn.execute("ALTER TABLE tags ADD COLUMN deadband REAL DEFAULT 0.0;", [])?;
    }
    if !has_col("data_points", "quality")? {
        conn.execute(
            "ALTER TABLE data_points ADD COLUMN quality INTEGER DEFAULT 0;",
            [],
        )?;
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
            log_interval_ms INT DEFAULT 0,
            deadband DOUBLE PRECISION DEFAULT 0.0
        );
        CREATE TABLE IF NOT EXISTS data_points (
            \"timestamp\" TIMESTAMPTZ NOT NULL,
            tag_id INT NOT NULL,
            value DOUBLE PRECISION NOT NULL,
            quality SMALLINT DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_dp_tag_ts ON data_points (tag_id, \"timestamp\" DESC);
        -- Отдельный индекс по времени: top-1 «последняя запись» и окна без
        -- фильтра по тегу (обзор архива) иначе сканируют всю таблицу.
        CREATE INDEX IF NOT EXISTS idx_dp_ts_only ON data_points (\"timestamp\" DESC);
        ALTER TABLE tags ADD COLUMN IF NOT EXISTS log_interval_ms INT DEFAULT 0;
        ALTER TABLE tags ADD COLUMN IF NOT EXISTS deadband DOUBLE PRECISION DEFAULT 0.0;
        ALTER TABLE data_points ADD COLUMN IF NOT EXISTS quality SMALLINT DEFAULT 0;
        -- Сводная таблица для быстрого прореживания окон > 1 минуты.
        -- Хранит среднее по минутному бакету: ~1000x меньше строк, чем data_points.
        -- avg_value — DOUBLE PRECISION: параметр f64 не кодируется в REAL
        -- (float4), INSERT падал молча; каст ::float8 при чтении ниже.
        CREATE TABLE IF NOT EXISTS data_points_summary (
            tag_id INT NOT NULL,
            \"bucket\" TIMESTAMPTZ NOT NULL,
            avg_value DOUBLE PRECISION NOT NULL,
            PRIMARY KEY (tag_id, \"bucket\")
        );
        CREATE INDEX IF NOT EXISTS idx_summary_tag_bucket ON data_points_summary (tag_id, \"bucket\" DESC);
        -- Миграция старых схем: REAL -> DOUBLE PRECISION.
        DO $$ BEGIN
            IF EXISTS (SELECT 1 FROM information_schema.columns
                       WHERE table_name = 'data_points_summary'
                         AND column_name = 'avg_value'
                         AND data_type = 'real') THEN
                ALTER TABLE data_points_summary ALTER COLUMN avg_value TYPE DOUBLE PRECISION;
            END IF;
        END $$;
        ",
    )
    .await
    .map_err(|e| format!("схема PostgreSQL: {e}"))?;
    Ok(())
}

/// TimescaleDB опционален: без него data_points остаётся обычной таблицей.
async fn try_create_timescale(client: &AsyncMutex<tokio_postgres::Client>) -> bool {
    let c = client.lock().await;
    let ext = c
        .batch_execute("CREATE EXTENSION IF NOT EXISTS timescaledb;")
        .await;
    if ext.is_err() {
        tracing::info!("TimescaleDB не доступен (data_points остаётся обычной таблицей)");
        return false;
    }
    let h = c
        .batch_execute(
            "SELECT create_hypertable('data_points', 'timestamp', if_not_exists => TRUE);",
        )
        .await;
    match h {
        Ok(_) => {
            tracing::info!("TimescaleDB hypertable создан/существует");
            enable_timescale_compression(&c).await;
            true
        }
        Err(e) => {
            tracing::info!("TimescaleDB hypertable не создан: {e}");
            false
        }
    }
}

/// Columnar-сжатие TimescaleDB для data_points.
///
/// Чанки старше COMPRESS_AFTER декомпозицией не читаются поодиночке:
/// segmentby по tag_id держит сигнал одного тега в одном сжатом фрагменте,
/// orderby по времени сохраняет исходный порядок для AVG-по-бакету.
/// Идемпотентно: повторное применение параметров и политики ничего не меняет.
async fn enable_timescale_compression(c: &tokio_postgres::Client) {
    // Направление orderby — ASC: все оконные запросы читают историю по возрастанию.
    let alter = c
        .batch_execute(
            "ALTER TABLE data_points SET (
                 timescaledb.compress,
                 timescaledb.compress_segmentby = 'tag_id',
                 timescaledb.compress_orderby = '\"timestamp\" ASC'
             );",
        )
        .await;
    if let Err(e) = alter {
        tracing::warn!("не удалось включить сжатие data_points: {e}");
        return;
    }
    // add_compression_policy без if_not_exists (в Timescale < 2.9 его нет)
    // падает на существующей базе — проверяем каталог заданий сам.
    let policy = c
        .batch_execute(
            "DO $$ BEGIN
                 IF NOT EXISTS (
                     SELECT 1 FROM timescaledb_information.jobs
                     WHERE hypertable_name = 'data_points' AND proc_name = 'policy_compression'
                 ) THEN
                     PERFORM add_compression_policy('data_points', INTERVAL '7 days');
                 END IF;
             END $$;",
        )
        .await;
    match policy {
        Ok(_) => tracing::info!("сжатие TimescaleDB включено (чанки старше 7 суток)"),
        Err(e) => tracing::warn!("политика сжатия не добавлена: {e}"),
    }
}

// --------------------------------------------------------------- строки -> Tag
/// Колонки SELECT для тегов.
const TAG_SELECT: &str = "id, connection_id, name, address_str, data_type, scale, offset_val, \
     unit, COALESCE(group_name, 'Общие') as group_name, \
     COALESCE(log_interval_ms, 0) as log_interval_ms, \
     COALESCE(deadband, 0.0) as deadband";

const TAG_SELECT_T: &str =
    "t.id, t.connection_id, t.name, t.address_str, t.data_type, t.scale, t.offset_val, \
     t.unit, COALESCE(t.group_name, 'Общие') as group_name, \
     COALESCE(t.log_interval_ms, 0) as log_interval_ms, \
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
        group_name: r
            .get::<_, Option<String>>(8)?
            .unwrap_or_else(|| "Общие".into()),
        log_interval_ms: r.get::<_, Option<i64>>(9)?.unwrap_or(0),
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
        group_name: r
            .get::<_, Option<String>>(8)
            .unwrap_or_else(|| "Общие".into()),
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
                let mut sql = String::from(
                    "INSERT INTO data_points (\"timestamp\", tag_id, value, quality) VALUES ",
                );
                // Колонки: TIMESTAMPTZ, INT, DOUBLE PRECISION, SMALLINT.
                // tokio-postgres сериализует параметр строго под тип, который ждёт
                // сервер: i64 принимает только INT8, i32 — только INT4. Без кастов
                // каждая пачка падала с "error serializing parameter 1" (tag_id i64 -> INT).
                let utc_times: Vec<DateTime<Utc>> = records
                    .iter()
                    .map(|(ts, ..)| ts.with_timezone(&Utc))
                    .collect();
                let tag_ids: Vec<i32> = records.iter().map(|(_, t, ..)| *t as i32).collect();
                let qualities: Vec<i16> = records.iter().map(|(_, _, _, q)| *q as i16).collect();
                let mut args: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    Vec::with_capacity(records.len() * 4);
                for (i, (_, _, value, _)) in records.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    let base = i * 4;
                    sql.push_str(&format!(
                        "(${}, ${}, ${}, ${})",
                        base + 1,
                        base + 2,
                        base + 3,
                        base + 4
                    ));
                    args.push(&utc_times[i]);
                    args.push(&tag_ids[i]);
                    args.push(value);
                    args.push(&qualities[i]);
                }
                c.execute(&sql, &args).await.map_err(|e| e.to_string())?;

                // Инкрементальное обновление сводной таблицы (минутные бакеты).
                // Группируем записи по (tag_id, minute_bucket), считаем среднее,
                // и upsert'им в data_points_summary. ON CONFLICT заменяет
                // предыдущее значение на новое среднее батча.
                if !records.is_empty() {
                    // Группируем по (tag_id, minute_bucket)
                    #[derive(Hash, Eq, PartialEq, Ord, PartialOrd)]
                    struct BucketKey {
                        tag_id: i32,
                        bucket: DateTime<Utc>,
                    }
                    let mut bucket_map: std::collections::BTreeMap<BucketKey, (f64, usize)> =
                        std::collections::BTreeMap::new();
                    for (ts, tag_id, value, _q) in records {
                        let utc = ts.with_timezone(&Utc);
                        // Убираем секунды и микросекунды → минутный бакет
                        let bucket = Utc
                            .with_ymd_and_hms(
                                utc.year(),
                                utc.month(),
                                utc.day(),
                                utc.hour(),
                                utc.minute(),
                                0,
                            )
                            .single()
                            .unwrap_or(utc);
                        let key = BucketKey {
                            tag_id: *tag_id as i32,
                            bucket,
                        };
                        let entry = bucket_map.entry(key).or_insert((0.0, 0));
                        entry.0 += value;
                        entry.1 += 1;
                    }
                    // Строим multi-row INSERT с ON CONFLICT REPLACE.
                    // Средние сначала materialize'им в owned-вектор: ссылки на
                    // временные значения внутри цикла не дожили бы до execute().
                    if !bucket_map.is_empty() {
                        let avgs: Vec<(i32, DateTime<Utc>, f64)> = bucket_map
                            .iter()
                            .map(|(key, &(sum, count))| {
                                (key.tag_id, key.bucket, sum / count as f64)
                            })
                            .collect();
                        let mut upsql = String::from(
                            "INSERT INTO data_points_summary (tag_id, \"bucket\", avg_value) VALUES "
                        );
                        let mut uargs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = Vec::new();
                        for (idx, (tag_id, bucket, avg)) in avgs.iter().enumerate() {
                            if idx > 0 {
                                upsql.push_str(", ");
                            }
                            let base = idx * 3;
                            upsql.push_str(&format!(
                                "(${}, ${}, ${})",
                                base + 1,
                                base + 2,
                                base + 3
                            ));
                            uargs.push(tag_id);
                            uargs.push(bucket);
                            uargs.push(avg);
                        }
                        upsql.push_str(
                            " ON CONFLICT (tag_id, \"bucket\") DO UPDATE SET avg_value = EXCLUDED.avg_value;"
                        );
                        if let Err(e) = c.execute(&upsql, &uargs).await {
                            // Ошибку не пробрасываем: summary — вспомогательная таблица,
                            // её устаревание не влияет на запись данных. Но и молчать
                            // нельзя: без лога broken upsert выглядел как «данные есть,
                            // но большие окна пустые».
                            tracing::warn!("summary upsert: {e}");
                        }
                    }
                }

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
                    .execute(
                        "DELETE FROM data_points WHERE timestamp < ?1;",
                        params![dt_str(&cutoff)],
                    )
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
                let rows = stmt
                    .query_map([], conn_from_irow)
                    .map_err(|e| e.to_string())?;
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
                conn.execute(
                    "DELETE FROM tag_groups WHERE connection_id = ?1;",
                    params![conn_id],
                )
                .map_err(|e| e.to_string())?;
                conn.execute("DELETE FROM connections WHERE id = ?1;", params![conn_id])
                    .map_err(|e| e.to_string())?;
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                c.execute(
                    "DELETE FROM tag_groups WHERE connection_id = $1;",
                    &[&(conn_id as i32)],
                )
                .await
                .map_err(|e| e.to_string())?;
                c.execute(
                    "DELETE FROM connections WHERE id = $1;",
                    &[&(conn_id as i32)],
                )
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
                let rows = stmt
                    .query_map([], tag_from_irow)
                    .map_err(|e| e.to_string())?;
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
                let rows = stmt
                    .query_map([], tag_from_irow)
                    .map_err(|e| e.to_string())?;
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
                let sql = format!(
                    "SELECT {TAG_SELECT} FROM tags WHERE connection_id = ?1 ORDER BY id ASC;"
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(params![conn_id], tag_from_irow)
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let sql = format!(
                    "SELECT {TAG_SELECT} FROM tags WHERE connection_id = $1 ORDER BY id ASC;"
                );
                let rows = c
                    .query(&sql[..], &[&(conn_id as i32)])
                    .await
                    .map_err(|e| e.to_string())?;
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
                        .prepare_cached(&format!(
                            "DELETE FROM data_points WHERE tag_id IN ({marks});"
                        ))
                        .map_err(|e| e.to_string())?;
                    let pv: Vec<&dyn rusqlite::types::ToSql> = chunk
                        .iter()
                        .map(|i| i as &dyn rusqlite::types::ToSql)
                        .collect();
                    stmt.execute(rusqlite::params_from_iter(pv.iter()))
                        .map_err(|e| e.to_string())?;
                    drop(stmt);
                    let mut stmt = conn
                        .prepare_cached(&format!("DELETE FROM tags WHERE id IN ({marks});"))
                        .map_err(|e| e.to_string())?;
                    stmt.execute(rusqlite::params_from_iter(pv.iter()))
                        .map_err(|e| e.to_string())?;
                }
                // SQLite не уменьшает файл автоматически после DELETE —
                // освобождаем место на диске.
                let _ = conn.execute("VACUUM", []);

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
                    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = int_ids
                        .iter()
                        .map(|i| i as &(dyn tokio_postgres::types::ToSql + Sync))
                        .collect();
                    c.execute(
                        &format!("DELETE FROM data_points WHERE tag_id IN ({marks});"),
                        &refs[..],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                    c.execute(
                        &format!("DELETE FROM tags WHERE id IN ({marks});"),
                        &refs[..],
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Пакетный перенос тегов в другую группу/подключение (drag&drop дерева I/O).
    async fn move_tags(
        &self,
        tag_ids: &[i64],
        target_conn_id: i64,
        target_group: &str,
    ) -> DbResult<()> {
        let mut ids: Vec<i64> = Vec::new();
        for id in tag_ids {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        if ids.is_empty() {
            return Ok(());
        }
        let group = if target_group.is_empty() {
            "Общие"
        } else {
            target_group
        };
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
                    stmt.execute(rusqlite::params_from_iter(pv.iter()))
                        .map_err(|e| e.to_string())?;
                }
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                for chunk in chunks(&ids, 500) {
                    let marks = (0..chunk.len())
                        .map(|i| format!("${}", i + 3))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let target = target_conn_id as i32;
                    let int_ids: Vec<i32> = chunk.iter().map(|i| *i as i32).collect();
                    let mut refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        vec![&group, &target];
                    refs.extend(
                        int_ids
                            .iter()
                            .map(|i| i as &(dyn tokio_postgres::types::ToSql + Sync)),
                    );
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
                let rows = stmt
                    .query_map(params![conn_id], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| e.to_string())?);
                }
                Ok(out)
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let rows = c
                    .query(psql, &[&(conn_id as i32)])
                    .await
                    .map_err(|e| e.to_string())?;
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
                    let g = if t.group_name.is_empty() {
                        "Общие"
                    } else {
                        &t.group_name
                    };
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
                    .query_map(params![conn_id], |r| {
                        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)? as usize))
                    })
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
                    .query(
                        "SELECT name FROM tag_groups WHERE connection_id = $1;",
                        &[&(conn_id as i32)],
                    )
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

    /// Точки за диапазон. Если их больше MAX_POINTS — отдаётся screen-space
    /// прореживание: окно режется на MAX_POINTS бакетов, по одной точке на
    /// бакет (агрегацию считает сама БД по индексу (tag_id, timestamp)).
    /// Так график на десятки миллионов строк читается за миллисекунды и не
    /// гоняет в ответ весь массив.
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
                let mut cnt = conn
                    .prepare_cached("SELECT COUNT(*) FROM data_points WHERE tag_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3;")
                    .map_err(|e| e.to_string())?;
                let total: i64 = cnt
                    .query_row(params![tag_id, dt_str(&start), dt_str(&end)], |r| r.get(0))
                    .unwrap_or(0);

                if (total as usize) <= MAX_POINTS {
                    return query_raw_sqlite(&conn, tag_id, &start, &end);
                }
                // Шаг бакета — целое число миллисекунд (см. ветку Postgres).
                let bucket_ms = bucket_ms((end - start).num_milliseconds());
                // Целочисленное деление после CAST AS INTEGER — тот же приём,
                // что в get_series_bucketed_sqlite. CAST AS REAL здесь не
                // сработает: от float к float это no-op, GROUP BY получил бы
                // по группе на строку и прореживания бы не было.
                // LIMIT — защита от переполнения, если фильтр по времени не сработает.
                let limit = MAX_POINTS + 100;
                let sql = format!(
                    "SELECT (CAST((julianday(timestamp) - 2440587.5) * 86400000 AS INTEGER) / ?4) * ?4, \
                     AVG(value) FROM data_points \
                     WHERE tag_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 \
                     GROUP BY 1 ORDER BY 1 \
                     LIMIT {limit};"
                );
                let mut stmt = conn.prepare_cached(&sql).map_err(|e| e.to_string())?;
                let rows: Vec<(f64, f64)> = stmt
                    .query_map(
                        params![tag_id, dt_str(&start), dt_str(&end), bucket_ms],
                        |r| Ok((r.get::<_, i64>(0)? as f64, r.get::<_, f64>(1)?)),
                    )
                    .map_err(|e| e.to_string())?
                    .filter_map(|r| r.ok())
                    .collect();
                let start_ms = start.timestamp_millis();
                let end_ms = end.timestamp_millis();
                Ok(densify_buckets(rows, start_ms, end_ms, bucket_ms))
            }
            Backend::Postgres(lock) => {
                let c = lock.lock().await;
                let (p_tag, p_start, p_end) = (
                    tag_id as i32,
                    start.with_timezone(&Utc),
                    end.with_timezone(&Utc),
                );
                let total: i64 = c
                    .query(
                        "SELECT COUNT(*) FROM data_points WHERE tag_id = $1 AND \"timestamp\" >= $2 AND \"timestamp\" <= $3;",
                        &[&p_tag, &p_start, &p_end],
                    )
                    .await
                    .map_err(|e| e.to_string())?
                    .first()
                    .and_then(|r| r.get::<_, Option<i64>>(0))
                    .unwrap_or(0);

                if (total as usize) <= MAX_POINTS {
                    let rows = c
                        .query(
                            "SELECT \"timestamp\", value FROM data_points WHERE tag_id = $1 AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 ORDER BY \"timestamp\" ASC;",
                            &[&p_tag, &p_start, &p_end],
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(rows
                        .iter()
                        .map(|r| {
                            let ts: DateTime<Utc> = r.get(0);
                            DataPoint {
                                timestamp: ts.with_timezone(&Local).naive_local(),
                                value: r.get(1),
                            }
                        })
                        .collect());
                }

                // Сводная таблица годится только когда шаг прореживания не
                // мельче минутного бакета — иначе в summary нет нужных
                // бакетов и densify вернул бы пустоту.
                let span_ms = (end - start).num_milliseconds();
                let bucket_step = bucket_ms(span_ms);
                if bucket_step >= SUMMARY_BUCKET_MS {
                    let rows: Vec<(f64, f64)> = c
                        .query(
                            "SELECT (EXTRACT(EPOCH FROM bucket) * 1000)::bigint, avg_value::float8 \
                             FROM data_points_summary \
                             WHERE tag_id = $1 AND bucket >= $2 AND bucket <= $3 \
                             ORDER BY bucket;",
                            &[&p_tag, &p_start, &p_end],
                        )
                        .await
                        .map_err(|e| e.to_string())?
                        .iter()
                        .map(|r| (r.get::<_, i64>(0) as f64, r.get(1)))
                        .collect();
                    let start_ms = start.timestamp_millis();
                    let end_ms = end.timestamp_millis();
                    return Ok(densify_buckets(rows, start_ms, end_ms, bucket_step));
                }

                // Прореживание на стороне БД: avg по бакету.
                // Целочисленная арифметика (bigint) вместо float — иначе
                // микросекундная точность TIMESTAMPTZ в PostgreSQL делает
                // floor(epoch_ms/bucket) несовпадающим между строками, и
                // GROUP BY не дедуплицирует. Результат: вместо 833 бакетов
                // возвращается 1250 (по одной на точку), и LOCF не срабатывает.
                // Каст $4::bigint обязателен: без него деление bigint/bigint
                // — целочисленное, а bigint/float8 → float8.
                // LIMIT — защита от переполнения, если фильтр по времени не сработает.
                let bucket_ms = bucket_ms(span_ms);
                let limit = (MAX_POINTS + 100) as i64;
                let sql = format!(
                    "SELECT (EXTRACT(EPOCH FROM \"timestamp\") * 1000)::bigint / $4::bigint * $4::bigint, AVG(value) \
                     FROM data_points \
                     WHERE tag_id = $1 AND \"timestamp\" >= $2 AND \"timestamp\" <= $3 \
                     GROUP BY 1 ORDER BY 1 \
                     LIMIT {limit};"
                );
                let rows: Vec<(f64, f64)> = c
                    .query(&sql, &[&p_tag, &p_start, &p_end, &bucket_ms])
                    .await
                    .map_err(|e| e.to_string())?
                    .iter()
                    // Колонка бакета — bigint (см. касты в SQL), читаем i64:
                    // deserializer tokio-postgres не конвертирует bigint в f64
                    // и паникует на r.get::<f64>.
                    .map(|r| (r.get::<_, i64>(0) as f64, r.get(1)))
                    .collect();
                let start_ms = start.timestamp_millis();
                let end_ms = end.timestamp_millis();
                Ok(densify_buckets(rows, start_ms, end_ms, bucket_ms))
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
            return Ok((
                Vec::new(),
                (now - chrono::Duration::milliseconds(delta_ms)).timestamp() as f64,
                now.timestamp() as f64,
            ));
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
        Ok((
            points,
            start_dt.timestamp() as f64,
            max_dt.timestamp() as f64,
        ))
    }
}
