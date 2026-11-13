//! REST-эндпоинты (аналог web_server.py FastAPI + API для вкладок IO/DB).
//!
//! Формат ответов повторяет Python-версию там, где её читает фронтенд:
//! /api/tags, /api/points/:id?span=, /api/archive/:id, /api/live.
//! Остальное — CRUD конфигурации и статусы, которые в desktop-версии делали
//! PyQt-виджеты напрямую через DatabaseService.

use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde_json::{json, Value};

use pda_core::database::db_service::{DatabaseService, DbConfig, DbConfigStore};
use pda_core::models::{Connection, Tag};

use crate::state::AppCore;

pub type ApiResult<T> = Result<T, ApiError>;

/// Ошибка API -> 500 + текст (Python бросал исключения так же).
pub struct ApiError(pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}

impl From<pda_core::database::db_service::DbError> for ApiError {
    fn from(e: pda_core::database::db_service::DbError) -> Self {
        ApiError(e)
    }
}

// ---------------------------------------------------------------- утилиты

/// Разбор текстового окна ("5 мин", "15 мин", "1 час", ...) по образцу
/// _parse_span_text из десктопного chart_widget.py. «15 мин» проверяется
/// раньше «5 мин» (в Python подстрока «5» есть в «15» — не копируем этот баг).
pub fn parse_span_ms(text: &str) -> i64 {
    let t = text.trim();
    if t.contains("15") && t.contains("мин") {
        return 15 * 60_000;
    }
    if t.contains("5") && t.contains("мин") {
        return 5 * 60_000;
    }
    if t.contains("1") && t.contains("час") && !t.contains("12") {
        return 3_600_000;
    }
    if t.contains("4") && t.contains("час") && !t.contains("24") {
        return 4 * 3_600_000;
    }
    if t.contains("12") {
        return 12 * 3_600_000;
    }
    if t.contains("72") {
        return 72 * 3_600_000;
    }
    if t.contains("24") {
        return 24 * 3_600_000;
    }
    3_600_000
}

/// datetime-local из браузера: "2026-10-09T12:30[:05]".
pub fn parse_iso_local(text: &str) -> Option<DateTime<Local>> {
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M"))
        .ok()?;
    Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
}

fn points_payload(points: Vec<pda_core::models::DataPoint>) -> Vec<(f64, f64)> {
    points
        .into_iter()
        .map(|p| {
            // Python: p.timestamp.timestamp() → float с микросекундами.
            // Rust: timestamp_millis() / 1000.0 — миллисекунды как float секунд.
            let ts = p
                .timestamp
                .and_local_timezone(Local)
                .single()
                .map(|d| d.timestamp_millis() as f64 / 1000.0)
                .unwrap_or(0.0);
            (ts, p.value)
        })
        .collect()
}

// ---------------------------------------------------------------- виртуалы
//
// Теги virtual-подключений не пишутся в журнал — их прошлое вычисляется из
// историй операндов на каждый запрос (/points, /archive, /series). Здесь
// сборка окна: замыкание зависимостей, чтение реальных серий и вызов
// pda_core::core::virtual_calc::eval_history.

/// Индексы конфигурации: виртуальные теги и принадлежность тегов подключениям.
struct VirtualIndex {
    /// tag_id -> исходный код выражения (виртуалы).
    virt: std::collections::HashMap<i64, String>,
    /// Все id тегов конфигурации (для проверки существования операнда).
    known: std::collections::HashSet<i64>,
}

async fn build_virtual_index(db: &Arc<DatabaseService>) -> ApiResult<VirtualIndex> {
    let conns = db.get_all_connections().await?;
    let virt_conns: std::collections::HashSet<i64> = conns
        .iter()
        .filter(|c| pda_core::drivers::registry::is_virtual(&c.driver_type))
        .map(|c| c.id)
        .collect();
    let tags = db.get_all_tags().await?;
    let virt = tags
        .iter()
        .filter(|t| virt_conns.contains(&t.connection_id))
        .map(|t| (t.id, t.address_str.clone()))
        .collect();
    let known = tags.iter().map(|t| t.id).collect();
    Ok(VirtualIndex { virt, known })
}

impl VirtualIndex {
    fn info(&self) -> impl Fn(i64) -> Option<(bool, String)> + '_ {
        |id| {
            if let Some(src) = self.virt.get(&id) {
                Some((true, src.clone()))
            } else if self.known.contains(&id) {
                Some((false, String::new()))
            } else {
                None
            }
        }
    }

    /// Замыкание виртуальных зависимостей: все реальные операнды (транзитивно
    /// через вложенные виртуалы), чью историю надо прочитать для запроса.
    fn operand_ids(&self, wanted: &[i64]) -> Vec<i64> {
        use pda_core::core::virtual_expr;
        let mut pending: Vec<i64> = wanted
            .iter()
            .copied()
            .filter(|id| self.virt.contains_key(id))
            .collect();
        let mut seen_virt: std::collections::HashSet<i64> = pending.iter().copied().collect();
        let mut real: std::collections::HashSet<i64> = std::collections::HashSet::new();
        while let Some(id) = pending.pop() {
            let src = match self.virt.get(&id) {
                Some(s) => s,
                None => {
                    real.insert(id); // реальный операнд
                    continue;
                }
            };
            let deps = virtual_expr::deps_of(src).unwrap_or_default();
            for d in deps {
                if self.virt.contains_key(&d) {
                    if seen_virt.insert(d) {
                        pending.push(d);
                    }
                } else {
                    real.insert(d);
                }
            }
        }
        real.into_iter().collect()
    }
    /// Максимальное окно AVG (мс) среди выражений wanted и их транзитивных
    /// виртуальных зависимостей. Историю операндов читают с таким запасом
    /// влево, чтобы среднее по окну считалось корректно на начале окна.
    fn avg_lead_ms(&self, wanted: &[i64]) -> i64 {
        use pda_core::core::virtual_expr;
        let mut pending: Vec<i64> = wanted
            .iter()
            .copied()
            .filter(|id| self.virt.contains_key(id))
            .collect();
        let mut seen: std::collections::HashSet<i64> = pending.iter().copied().collect();
        let mut lead = 0.0f64;
        while let Some(id) = pending.pop() {
            let src = match self.virt.get(&id) {
                Some(s) => s,
                None => continue,
            };
            let node = match virtual_expr::parse(src) {
                Ok(n) => n,
                Err(_) => continue,
            };
            lead = lead.max(virtual_expr::max_window_ms(&node));
            let mut refs = std::collections::HashSet::new();
            virtual_expr::dependencies(&node, &mut refs);
            for d in refs {
                if self.virt.contains_key(&d) && seen.insert(d) {
                    pending.push(d);
                }
            }
        }
        lead.ceil() as i64
    }
}

/// Истории виртуалов `wanted` за окно [start, end].
/// Пустой результат, если ни один из wanted не виртуальный.
async fn virtual_series(
    db: &Arc<DatabaseService>,
    idx: &VirtualIndex,
    wanted: &[i64],
    start: DateTime<Local>,
    end: DateTime<Local>,
) -> ApiResult<Vec<(i64, Vec<(f64, f64)>)>> {
    let wanted_virt: Vec<i64> = wanted.iter().copied().filter(|id| idx.virt.contains_key(id)).collect();
    if wanted_virt.is_empty() {
        return Ok(Vec::new());
    }
    // Реальные операнды читаем сырыми точками (группировку по бакетам
    // делает eval_history на объединённой сетке).
    //
    // Левый край расширяем на максимальное окно AVG (из истории операнда),
    // иначе среднее на начале окна считалось бы по неполному числу точек.
    // Правый край — на 1 мс: журнал хранит время с микросекундами
    // (...26.958666), а границы окон (anchor из timestamp_millis, t0/t1 фронта)
    // усечены до мс (...26.958). SQLite сравнивает timestamps как строки, и
    // точка ровно на границе оказалась бы за окном — на live-графике это
    // незаметно (поток непрерывен), но последний шаг истории пропадал.
    let lead_ms = idx.avg_lead_ms(&wanted_virt).max(0);
    let start_read = start - chrono::Duration::milliseconds(lead_ms);
    let end_read = end + chrono::Duration::milliseconds(1);
    let mut real = std::collections::BTreeMap::new();
    for id in idx.operand_ids(&wanted_virt) {
        let rows = db.get_data_points_range(id, start_read, end_read).await?;
        let pts: Vec<(i64, f64)> = rows
            .into_iter()
            .map(|p| {
                let ms = p
                    .timestamp
                    .and_local_timezone(Local)
                    .single()
                    .map(|d| d.timestamp_millis())
                    .unwrap_or(0);
                (ms, p.value)
            })
            .collect();
        real.insert(id, pts);
    }
    let out = pda_core::core::virtual_calc::eval_history(&wanted_virt, &idx.info(), &real);
    let start_ms = start.timestamp_millis();
    Ok(out
        .into_iter()
        .map(|(id, s)| {
            (
                id,
                s.into_iter()
                    .filter(|(t, _)| *t >= start_ms)
                    .map(|(t, v)| (t as f64 / 1000.0, v))
                    .collect(),
            )
        })
        .collect())
}

//serde default-хелперы (замыкания в attr не допускаются)
fn default_span() -> String {
    "1 час".to_string()
}
fn default_poll() -> i64 {
    100
}
fn default_dtype() -> String {
    "FLOAT".to_string()
}

// ---------------------------------------------------------------- чтение

/// Список тегов с именами подключений (формат Python /api/tags).
pub async fn tags(State(core): State<Arc<AppCore>>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let tags = db.get_all_tags().await?;
    let conns = db.get_all_connections().await?;
    let name_of = |cid: i64| {
        conns
            .iter()
            .find(|c| c.id == cid)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    };
    let out: Vec<Value> = tags
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "name": t.name,
                "address": t.address_str,
                "unit": t.unit,
                "data_type": t.data_type,
                "group": if t.group_name.is_empty() { "Общие".to_string() } else { t.group_name.clone() },
                "connection": name_of(t.connection_id),
                "connection_id": t.connection_id,
            })
        })
        .collect();
    Ok(Json(json!(out)))
}

#[derive(serde::Deserialize)]
pub struct SpanQuery {
    #[serde(default = "default_span")]
    span: String,
}

/// Точки за окно «span» относительно последней записи (Python /api/points).
/// Виртуальные теги: окно берётся относительно последней записи операндов,
/// значения вычисляются из их историй.
pub async fn points(
    State(core): State<Arc<AppCore>>,
    UrlPath(tag_id): UrlPath<i64>,
    Query(q): Query<SpanQuery>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let delta_ms = parse_span_ms(&q.span);
    let idx = build_virtual_index(&db).await?;
    if idx.virt.contains_key(&tag_id) {
        // Опора — самая свежая запись операндов (у виртуала своей истории нет).
        let operands = idx.operand_ids(&[tag_id]);
        let mut anchor_ms: Option<i64> = None;
        for op in &operands {
            if let Ok(ts) = db.get_last_data_time_for(*op).await {
                if let Some(ms) = ts {
                    anchor_ms = Some(anchor_ms.map_or(ms, |a: i64| a.max(ms)));
                }
            }
        }
        let end_dt = chrono::TimeZone::timestamp_millis_opt(&Local, anchor_ms.unwrap_or_else(|| Local::now().timestamp_millis()))
            .single()
            .unwrap_or_else(Local::now);
        let start_dt = end_dt - chrono::Duration::milliseconds(delta_ms);
        let series = virtual_series(&db, &idx, &[tag_id], start_dt, end_dt).await?;
        let (times, values): (Vec<f64>, Vec<f64>) = series
            .into_iter()
            .next()
            .map(|(_, s)| s)
            .unwrap_or_default()
            .into_iter()
            .unzip();
        return Ok(Json(json!({
            "times": times,
            "values": values,
            "t_start": start_dt.timestamp() as f64,
            "t_end": end_dt.timestamp() as f64,
        })));
    }
    let (points, t_start, t_end) = db.get_data_points_by_span(tag_id, delta_ms).await?;
    let (times, values): (Vec<f64>, Vec<f64>) = points_payload(points).into_iter().unzip();
    Ok(Json(
        json!({ "times": times, "values": values, "t_start": t_start, "t_end": t_end }),
    ))
}

#[derive(serde::Deserialize)]
pub struct ArchiveQuery {
    /// Начало окна (datetime-local). Не нужен, если заданы t0/t1.
    #[serde(default)]
    start: String,
    #[serde(default = "default_span")]
    duration: String,
    /// Точное окно в epoch-секундах (выбор на мини-обзоре архива).
    /// Когда заданы оба — имеют приоритет над start/duration.
    t0: Option<f64>,
    t1: Option<f64>,
}

/// Исторический диапазон (Python /api/archive).
pub async fn archive(
    State(core): State<Arc<AppCore>>,
    UrlPath(tag_id): UrlPath<i64>,
    Query(q): Query<ArchiveQuery>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let (start_dt, end_dt) = match (q.t0, q.t1) {
        (Some(t0), Some(t1)) if t1 > t0 => {
            let start_dt = chrono::TimeZone::timestamp_millis_opt(&Local, (t0 * 1000.0).round() as i64)
                .single()
                .unwrap_or_else(|| Local::now() - chrono::Duration::hours(1));
            let end_dt = chrono::TimeZone::timestamp_millis_opt(&Local, (t1 * 1000.0).round() as i64)
                .single()
                .unwrap_or_else(|| Local::now());
            (start_dt, end_dt)
        }
        _ => {
            let start_dt = parse_iso_local(&q.start)
                .unwrap_or_else(|| Local::now() - chrono::Duration::hours(1));
            let dur_ms = parse_span_ms(&q.duration);
            (start_dt, start_dt + chrono::Duration::milliseconds(dur_ms))
        }
    };
    let idx = build_virtual_index(&db).await?;
    if idx.virt.contains_key(&tag_id) {
        let series = virtual_series(&db, &idx, &[tag_id], start_dt, end_dt).await?;
        let (times, values): (Vec<f64>, Vec<f64>) = series
            .into_iter()
            .next()
            .map(|(_, s)| s)
            .unwrap_or_default()
            .into_iter()
            .unzip();
        return Ok(Json(json!({
            "times": times,
            "values": values,
            "t_start": start_dt.timestamp() as f64,
            "t_end": end_dt.timestamp() as f64,
        })));
    }
    let points = db.get_data_points_range(tag_id, start_dt, end_dt).await?;
    let (times, values): (Vec<f64>, Vec<f64>) = points_payload(points).into_iter().unzip();
    Ok(Json(json!({
        "times": times,
        "values": values,
        "t_start": start_dt.timestamp() as f64,
        "t_end": end_dt.timestamp() as f64,
    })))
}

/// Пакетное чтение истории для окна графика.
///
/// Возвращает выровненные серии: все теги получают одинаковую сетку времени,
/// иначе uPlot сдвинет значения одного сигнала относительно другого.
/// Решение о прореживании принимается один раз на окно, а не на тег.
#[derive(serde::Deserialize)]
pub struct SeriesQuery {
    /// Теги через запятую.
    tags: String,
    /// Окно относительно последней записи (Live). Игнорируется, если есть start.
    #[serde(default = "default_span")]
    span: String,
    /// Начало окна (Archive).
    start: Option<String>,
    /// Длительность окна (Archive).
    duration: Option<String>,
    /// Явное окно зума: начало и конец в epoch-секундах (float). Когда заданы
    /// оба, имеют приоритет над start/duration/span — фронт присылает их после
    /// setScale, чтобы не зависеть от текстовых форматов span.
    t0: Option<f64>,
    t1: Option<f64>,
}

/// Метка самой свежей записи в базе (epoch, сек. float) — /api/last.
/// Режим Archive показывает её во фронтенде, чтобы пользователь видел,
/// до какого момента вообще есть история.
pub async fn last_data(State(core): State<Arc<AppCore>>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let t_last = db.get_last_data_time().await?;
    Ok(Json(json!({
        "t_last": t_last.map(|ms| ms as f64 / 1000.0),
    })))
}

/// Пустые интервалы истории (для подсветки «дыр» на обзоре архива).
///
/// Возвращает интервалы БЕЗ данных по выбранным тегам: сетка окна режется на
/// `buckets` бакетов, занятые бакеты находятся одним DISTINCT-запросом, а
/// промежутки между ними склеиваются здесь. Ответ маленький (десятки пар
/// чисел) при любом окне, поэтому обзор суток не тормозит.
#[derive(serde::Deserialize)]
pub struct GapsQuery {
    tags: String,
    t0: f64,
    t1: f64,
    /// Число бакетов сетки (по умолчанию 600 — на сутки это ~2.4 мин).
    buckets: Option<usize>,
}

pub async fn data_gaps(
    State(core): State<Arc<AppCore>>,
    Query(q): Query<GapsQuery>,
) -> ApiResult<Json<Value>> {
    let tag_ids: Vec<i64> = q
        .tags
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    if tag_ids.is_empty() || !(q.t1 > q.t0) {
        return Ok(Json(json!({ "gaps": [], "bucket_ms": 0 })));
    }
    let start_ms = (q.t0 * 1000.0).round() as i64;
    let end_ms = (q.t1 * 1000.0).round() as i64;
    let buckets = q.buckets.unwrap_or(600).clamp(20, 2000) as i64;
    let span_ms = (end_ms - start_ms).max(1);
    // Шаг бакета — целое число миллисекунд: дробный разъехался бы с сеткой БД.
    let bucket_ms = ((span_ms + buckets - 1) / buckets).max(1);

    let start_dt = chrono::TimeZone::timestamp_millis_opt(&Local, start_ms)
        .single()
        .unwrap_or_else(Local::now);
    let end_dt = chrono::TimeZone::timestamp_millis_opt(&Local, end_ms)
        .single()
        .unwrap_or_else(Local::now);

    let db = core.db_snapshot().await;
    let busy = db
        .get_busy_buckets(&tag_ids, start_dt, end_dt, bucket_ms)
        .await?;
    let busy: std::collections::HashSet<i64> = busy.into_iter().collect();

    // Обходим сетку и склеиваем подряд идущие пустые бакеты в один интервал.
    let mut gaps: Vec<[f64; 2]> = Vec::new();
    let mut run: Option<i64> = None;
    let mut b = start_ms.div_euclid(bucket_ms) * bucket_ms;
    while b < end_ms {
        if busy.contains(&b) {
            if let Some(from) = run.take() {
                gaps.push([from as f64 / 1000.0, b.min(end_ms) as f64 / 1000.0]);
            }
        } else if run.is_none() {
            run = Some(b.max(start_ms));
        }
        b += bucket_ms;
    }
    if let Some(from) = run {
        gaps.push([from as f64 / 1000.0, end_ms as f64 / 1000.0]);
    }

    Ok(Json(json!({
        "gaps": gaps,
        "bucket_ms": bucket_ms,
        "t0": q.t0,
        "t1": q.t1,
    })))
}

pub async fn series(
    State(core): State<Arc<AppCore>>,
    Query(q): Query<SeriesQuery>,
) -> ApiResult<Json<Value>> {
    let tag_ids: Vec<i64> = q
        .tags
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    if tag_ids.is_empty() {
        return Ok(Json(json!({ "series": [], "t_start": 0.0, "t_end": 0.0 })));
    }

    let db = core.db_snapshot().await;
    // Приоритет: явное окно зума (t0/t1) -> start+duration -> Live по span.
    // t0/t1 приходят в секундах (float) из uPlot; переводим в миллисекунды —
    // разрешение журнала всё равно мс, а наносекундная арифметика опасна
    // округлением (nanos == 1_000_000_000 -> None, и окно молча съехал бы).
    let (start_dt, end_dt) = match (q.t0, q.t1) {
        (Some(t0), Some(t1)) if t1 > t0 => {
            let start_dt =
                chrono::TimeZone::timestamp_millis_opt(&Local, (t0 * 1000.0).round() as i64)
                    .single()
                    .unwrap_or_else(|| Local::now() - chrono::Duration::hours(1));
            let end_dt =
                chrono::TimeZone::timestamp_millis_opt(&Local, (t1 * 1000.0).round() as i64)
                    .single()
                    .unwrap_or_else(|| Local::now());
            (start_dt, end_dt)
        }
        _ => match (&q.start, &q.duration) {
            (Some(s), Some(d)) => {
                let start_dt =
                    parse_iso_local(s).unwrap_or_else(|| Local::now() - chrono::Duration::hours(1));
                (
                    start_dt,
                    start_dt + chrono::Duration::milliseconds(parse_span_ms(d)),
                )
            }
            _ => {
                // Live: окно относительно последней записи самого свежего тега.
                let delta_ms = parse_span_ms(&q.span);
                let now = Local::now();
                (now - chrono::Duration::milliseconds(delta_ms), now)
            }
        },
    };

    // Виртуальные теги считаем из историй их операндов; реальные операнды
    // добавляем к окну чтения, чтобы не делать второй заход в БД.
    let idx = build_virtual_index(&db).await?;
    let virt_wanted: Vec<i64> = tag_ids.iter().copied().filter(|id| idx.virt.contains_key(id)).collect();
    let real_wanted: Vec<i64> = tag_ids.iter().copied().filter(|id| !idx.virt.contains_key(id)).collect();
    let mut read_ids = real_wanted.clone();
    // Для виртуалов с AVG читаем операнды с запасом влево на размер окна.
    let virt_start = if virt_wanted.is_empty() {
        start_dt
    } else {
        let lead_ms = idx.avg_lead_ms(&virt_wanted).max(0);
        start_dt - chrono::Duration::milliseconds(lead_ms)
    };
    if !virt_wanted.is_empty() {
        for op in idx.operand_ids(&virt_wanted) {
            if !read_ids.contains(&op) {
                read_ids.push(op);
            }
        }
    }

    let window = if read_ids.is_empty() {
        Vec::new()
    } else {
        db.get_series_window(&read_ids, virt_start, end_dt).await?
    };
    // Точки операндов до virt_start нужны только для AVG — наружу (в реальные
    // серии) они не попадают, window фильтруется ниже по real_wanted.
    let start_ms = start_dt.timestamp_millis();
    let mut series: Vec<Value> = window
        .iter()
        .filter(|(id, _)| real_wanted.contains(id))
        .map(|(tag_id, points)| {
            let (times, values): (Vec<f64>, Vec<f64>) = points_payload(points.clone()).into_iter().unzip();
            json!({ "tag_id": tag_id, "times": times, "values": values })
        })
        .collect();

    if !virt_wanted.is_empty() {
        // Сырые точки операндов из окна чтения (уже прочитано выше).
        let mut real_hist: std::collections::BTreeMap<i64, Vec<(i64, f64)>> = Default::default();
        for (id, points) in &window {
            if !real_wanted.contains(id) {
                let pts = points_payload(points.clone())
                    .into_iter()
                    .map(|(sec, v)| ((sec * 1000.0).round() as i64, v))
                    .collect();
                real_hist.insert(*id, pts);
            }
        }
        let virt_out = pda_core::core::virtual_calc::eval_history(
            &virt_wanted,
            &idx.info(),
            &real_hist,
        );
        for (tag_id, s) in virt_out {
            let (times, values): (Vec<f64>, Vec<f64>) = s
                .into_iter()
                .filter(|(t, _)| *t >= start_ms)
                .map(|(t, v)| (t as f64 / 1000.0, v))
                .unzip();
            series.push(json!({ "tag_id": tag_id, "times": times, "values": values }));
        }
    }

    // t_last — момент самой свежей записи во всей базе. Фронт в режиме Archive
    // показывает его рядом с полем «Начало»: видно, до какой даты грузить.
    let t_last = db.get_last_data_time().await.ok().flatten();
    Ok(Json(json!({
        "series": series,
        "t_start": start_dt.timestamp_millis() as f64 / 1000.0,
        "t_end": end_dt.timestamp_millis() as f64 / 1000.0,
        "t_last": t_last.map(|ms| ms as f64 / 1000.0),
    })))
}

/// Текущие значения из реестра (Python /api/live), с именами для таблиц IO.
pub async fn live(State(core): State<Arc<AppCore>>) -> Json<Value> {
    let rows: Vec<Value> = core
        .registry
        .live_snapshot()
        .into_iter()
        .map(|lv| {
            let name = core
                .registry
                .get(lv.tag_id)
                .map(|p| p.name())
                .unwrap_or_default();
            json!({
                "id": lv.tag_id,
                "name": name,
                "value": lv.value,
                "quality": lv.quality,
                "timestamp": lv.timestamp.map(|t| t.timestamp() as f64),
            })
        })
        .collect();
    Json(json!(rows))
}

// ---------------------------------------------------------------- статусы

/// Панель статуса: драйверы, журнал, версия (шапка окон + вкладки IO/DB).
pub async fn status(State(core): State<Arc<AppCore>>) -> Json<Value> {
    let mgr = core.manager.read().await;
    let db = core.db_snapshot().await;
    // Размер базы и число тегов для панели статуса (вкладка Database).
    // На недоступной БД поля остаются null/0 — статус-панель не должна падать.
    let (size_bytes, tags) = match db.get_db_info().await {
        Ok(v) => v,
        Err(_) => (None, 0),
    };
    Json(json!({
        "app": { "name": pda_core::config::APP_NAME, "version": pda_core::config::APP_VERSION },
        "drivers": mgr.drivers_json(),
        "manually_stopped": mgr.manually_stopped_ids(),
        "running": mgr.running_count(),
        "view_only": core.is_view_only(),
        "journal": mgr.journal_stats(),
        "db": {
            "available": db.is_available,
            "engine": format!("{:?}", db.config().engine).to_lowercase(),
            "error": db.last_error.clone(),
            "config": db.config().to_json(),
            "size_bytes": size_bytes,
            "tags": tags,
        },
    }))
}

#[derive(serde::Deserialize)]
pub struct RetentionBody {
    pub days: f64,
}

pub async fn set_retention(
    State(core): State<Arc<AppCore>>,
    Json(b): Json<RetentionBody>,
) -> Json<Value> {
    let days = b.days.max(0.0);
    {
        let mgr = core.manager.read().await;
        mgr.set_retention_days(days);
    }
    persist_retention(&core, days);
    Json(json!({ "ok": true, "days": days }))
}

/// Обновить ключ retention_days в db_config.json (read-patch-write): глубина
/// хранения переживает рестарт процесса. Нет файла — создаём с одним ключом,
/// остальное при старте возьмётся из дефолтов DbConfig.
fn persist_retention(core: &AppCore, days: f64) {
    let path = &core.db_config_path;
    let mut v = std::fs::read_to_string(path)
        .ok()
        .and_then(|txt| serde_json::from_str::<Value>(&txt).ok())
        .unwrap_or_else(|| json!({}));
    if !v.is_object() {
        v = json!({});
    }
    v["retention_days"] = json!(days);
    if let Err(e) = std::fs::write(path, serde_json::to_string_pretty(&v).unwrap()) {
        tracing::warn!("Не удалось сохранить retention в {}: {e}", path.display());
    }
}

// ---------------------------------------------------------------- режим опроса

#[derive(serde::Deserialize)]
pub struct PollingBody {
    /// true — опрос включён, false — «только просмотр» (опрос остановлен).
    pub enabled: bool,
}

/// Включить/остановить опрос всех модулей.
///
/// Остановка нужна, чтобы запустить вторую копию приложения рядом с рабочей:
/// она читает историю и рисует графики, но не опрашивает ПЛК и не занимает
/// порт встроенного Modbus-сервера.
pub async fn set_polling(
    State(core): State<Arc<AppCore>>,
    Json(b): Json<PollingBody>,
) -> Json<Value> {
    let view_only = core.set_view_only(!b.enabled).await;
    let running = core.manager.read().await.running_count();
    Json(json!({
        "ok": true,
        "polling": !view_only,
        "view_only": view_only,
        "running": running,
    }))
}

// ---------------------------------------------------------------- IO: подключения

pub async fn connections(State(core): State<Arc<AppCore>>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let conns = db.get_all_connections().await?;
    Ok(Json(
        serde_json::to_value(&conns).map_err(|e| ApiError(e.to_string()))?,
    ))
}

/// Форма подключения (создание — без id; изменение — PUT /connections/:id).
#[derive(serde::Deserialize)]
pub struct ConnBody {
    pub name: String,
    pub driver_type: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_poll")]
    pub poll_interval_ms: i64,
    #[serde(default)]
    pub config: Value,
}

fn conn_from_body(id: i64, b: ConnBody) -> Connection {
    let config = match b.config {
        Value::Object(map) => map.into_iter().collect(),
        _ => Default::default(),
    };
    Connection::new(
        id,
        b.name,
        b.driver_type,
        b.enabled,
        b.poll_interval_ms,
        config,
    )
}

pub async fn add_connection(
    State(core): State<Arc<AppCore>>,
    Json(b): Json<ConnBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let conn = conn_from_body(0, b);
    let id = db.add_connection(&conn).await?;
    core.manager.write().await.restart_all().await;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_connection(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
    Json(b): Json<ConnBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let conn = conn_from_body(id, b);
    db.update_connection(&conn).await?;
    // Python: после редактирования подключения — restart_all всего стека.
    core.manager.write().await.restart_all().await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_connection(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.delete_connection(id).await?;
    {
        let mut mgr = core.manager.write().await;
        mgr.restart_all().await;
        // Реестр не должен хранить точки удалённого модуля даже после сбоя запуска.
        mgr.registry.forget_connection(id);
        // Удалённый модуль вычёркивается из сохранённых ручных пауз (id больше нет).
        mgr.forget_connection(id);
    }
    Ok(Json(json!({ "ok": true })))
}

/// Перезапуск одного модуля (Python: «Перезапустить» в контекстном меню).
pub async fn restart_connection(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Json<Value>> {
    core.manager.write().await.restart_connection(id).await;
    Ok(Json(json!({ "ok": true })))
}

/// Остановить опрос одного модуля, не трогая остальные (дерево I/O: «⏸»).
pub async fn stop_connection(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Json<Value>> {
    let ok = core.manager.write().await.stop_connection(id).await;
    Ok(Json(json!({ "ok": ok, "manually_stopped": true })))
}

/// Запустить опрос одного модуля (снимает ручную паузу; дерево I/O: «▶»).
pub async fn start_connection(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Json<Value>> {
    let ok = core.manager.write().await.start_connection(id).await;
    Ok(Json(json!({ "ok": ok })))
}

// ---------------------------------------------------------------- IO: группы

#[derive(serde::Deserialize)]
pub struct GroupBody {
    pub name: String,
}

#[derive(serde::Deserialize)]
pub struct RenameBody {
    pub old: String,
    pub new: String,
}

#[derive(serde::Deserialize)]
pub struct DeleteGroupBody {
    pub name: String,
    #[serde(default)]
    pub with_tags: bool,
}

pub async fn list_groups(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let groups = db.get_groups_by_connection(id).await?;
    let counts = db.get_group_counts(id).await?;
    Ok(Json(json!({
        "groups": groups,
        "counts": counts.into_iter().map(|(g, n)| json!({"group": g, "tags": n})).collect::<Vec<_>>(),
    })))
}

pub async fn add_group(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
    Json(b): Json<GroupBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.add_group(id, &b.name).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn rename_group(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
    Json(b): Json<RenameBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.rename_group(id, &b.old, &b.new).await?;
    core.manager.read().await.reload_tags().await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_group(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
    Json(b): Json<DeleteGroupBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.delete_group(id, &b.name, b.with_tags).await?;
    core.manager.read().await.reload_tags().await;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- IO: теги

#[derive(serde::Deserialize)]
pub struct IdsBody {
    pub ids: Vec<i64>,
}

pub async fn tags_by_conn(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let tags = db.get_tags_by_connection(id).await?;
    Ok(Json(
        serde_json::to_value(&tags).map_err(|e| ApiError(e.to_string()))?,
    ))
}

pub async fn add_tag(
    State(core): State<Arc<AppCore>>,
    Json(tag): Json<Tag>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let mut tag = tag;
    tag.id = 0;
    let id = db.add_tag(&tag).await?;
    core.manager.read().await.reload_tags().await;
    Ok(Json(json!({ "id": id })))
}

pub async fn update_tag(
    State(core): State<Arc<AppCore>>,
    UrlPath(id): UrlPath<i64>,
    Json(tag): Json<Tag>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let mut tag = tag;
    tag.id = id;
    db.update_tag(&tag).await?;
    core.manager.read().await.reload_tags().await;
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_tags(
    State(core): State<Arc<AppCore>>,
    Json(b): Json<IdsBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.delete_tags(&b.ids).await?;
    core.manager.read().await.reload_tags().await;
    Ok(Json(json!({ "ok": true })))
}

#[derive(serde::Deserialize)]
pub struct MoveTagsBody {
    pub ids: Vec<i64>,
    pub target_connection_id: i64,
    #[serde(default)]
    pub target_group: String,
}

/// Перенос переменных в другую группу/модуль (drag&drop дерева I/O).
pub async fn move_tags(
    State(core): State<Arc<AppCore>>,
    Json(b): Json<MoveTagsBody>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.move_tags(&b.ids, b.target_connection_id, &b.target_group)
        .await?;
    core.manager.read().await.reload_tags().await;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- справочник драйверов

/// Типы драйверов + подписи + подсказки адресов (конструкторы IO-вкладки).
pub async fn driver_info() -> Json<Value> {
    let types: Vec<Value> = pda_core::drivers::registry::driver_type_keys()
        .into_iter()
        .map(|k| {
            json!({
                "key": k,
                "label": pda_core::drivers::registry::get_driver_label(k),
                "address": pda_core::drivers::registry::address_info(k),
            })
        })
        .collect();
    Json(json!({ "types": types }))
}

#[derive(serde::Deserialize)]
pub struct ValidateBody {
    pub driver_type: String,
    pub address: String,
    #[serde(default = "default_dtype")]
    pub data_type: String,
}

pub async fn validate_address(Json(b): Json<ValidateBody>) -> Json<Value> {
    let err =
        pda_core::drivers::registry::validate_address(&b.driver_type, &b.address, &b.data_type);
    Json(json!({ "ok": err.is_none(), "error": err }))
}

// ---------------------------------------------------------------- База данных

pub async fn db_status(State(core): State<Arc<AppCore>>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    // Размер и число тегов — для панели статуса. На недоступной базе не
    // рвём ответ: поля просто остаются null/0.
    let (size_bytes, tags) = match db.get_db_info().await {
        Ok(v) => v,
        Err(_) => (None, 0),
    };
    Ok(Json(json!({
        "available": db.is_available,
        "error": db.last_error.clone(),
        "config": db.config().to_json(),
        "size_bytes": size_bytes,
        "tags": tags,
    })))
}

#[derive(serde::Deserialize)]
pub struct DbSwitchBody {
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub sqlite_path: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub dbname: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

impl DbSwitchBody {
    fn into_cfg(&self) -> DbConfig {
        // python DatabaseService(**cfg): поле engine, остальное по движку
        DbConfig::from_json(&json!({
            "engine": self.engine,
            "sqlite_path": self.sqlite_path,
            "host": self.host,
            "port": self.port,
            "dbname": self.dbname,
            "user": self.user,
            "password": self.password,
        }))
    }

    fn to_json(&self) -> Value {
        json!({
            "engine": self.engine,
            "sqlite_path": self.sqlite_path,
            "host": self.host,
            "port": self.port,
            "dbname": self.dbname,
            "user": self.user,
        })
    }
}

/// Тест соединения: открываем кандидат и тут же закрываем.
pub async fn db_test(Json(b): Json<DbSwitchBody>) -> Json<Value> {
    let candidate = DatabaseService::open(b.into_cfg()).await;
    match &candidate.last_error {
        Some(err) => Json(json!({ "ok": false, "error": err })),
        None => {
            let n = candidate
                .get_all_connections()
                .await
                .map(|c| c.len())
                .unwrap_or(0);
            Json(json!({ "ok": true, "connections": n }))
        }
    }
}

/// Создать целевую базу (аналог Python create_database). SQLite создаётся
/// автоматически при применении; MariaDB/MySQL пока не перенесены.
pub async fn db_create(Json(b): Json<DbSwitchBody>) -> ApiResult<Json<Value>> {
    match b.engine.as_str() {
        "sqlite" => Ok(Json(
            json!({ "ok": true, "created": false, "info": "Для SQLite база создаётся автоматически при применении подключения." }),
        )),
        "postgres" => {
            let cfg = b.into_cfg();
            match DatabaseService::create_postgres_database(&cfg).await {
                Ok(created) => Ok(Json(json!({ "ok": true, "created": created }))),
                Err(e) => Err(ApiError(e)),
            }
        }
        _ => Err(ApiError(
            "Создание базы через GUI доступно только для PostgreSQL".into(),
        )),
    }
}

/// Применить и переключить (Python _save_and_reconnect): пишем db_config.json,
/// останавливаем драйверы, открываем новую базу, перезапускаем.
pub async fn db_switch(
    State(core): State<Arc<AppCore>>,
    Json(b): Json<DbSwitchBody>,
) -> ApiResult<Json<Value>> {
    let body_cfg = b.to_json();
    let cfg = b.into_cfg();
    core.switch_database(cfg).await.map_err(ApiError)?;
    // retention_days — настройка не из формы БД; сохраняем текущую, чтобы
    // переключение базы не сбрасывало глубину хранения.
    let retention = core
        .manager
        .read()
        .await
        .journal_stats()["retention_days"]
        .as_f64()
        .unwrap_or(0.0);
    let mut file_cfg = body_cfg;
    file_cfg["retention_days"] = json!(retention);
    if let Err(e) = std::fs::write(
        &core.db_config_path,
        serde_json::to_string_pretty(&file_cfg).unwrap(),
    ) {
        tracing::warn!(
            "Не удалось сохранить {}: {e}",
            core.db_config_path.display()
        );
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- экспорт PNG

/// Сигнатура PNG-файла — проверяем, чтобы в каталог выгрузки не записался мусор.
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Разумный предел: график 4K в PNG весит единицы мегабайт.
const MAX_PNG_BYTES: usize = 64 * 1024 * 1024;

#[derive(serde::Deserialize)]
pub struct ExportNameQuery {
    #[serde(default)]
    name: Option<String>,
}

/// Разделители пути: обратный слэш через unicode-escape, чтобы не зависеть
/// от экранирования в исходнике.
const SEPARATORS: [char; 2] = ['/', '\u{5c}'];

/// Безопасное имя файла для сохранения: вырезается путь (в том числе с
/// обратным слэшем), остаются только латиница/цифры/._-, расширение всегда .png.
fn safe_export_name(raw: Option<&str>, ext: &str) -> String {
    let ext = ext.trim_start_matches('.').to_lowercase();
    let base = raw
        .unwrap_or_default()
        .rsplit(SEPARATORS)
        .next()
        .unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    // Убираем любое «хвостовое» расширение, чтобы не получить «name.pdf.png».
    let stem = cleaned
        .trim_matches('.')
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(&cleaned)
        .trim_matches('.');
    // Ограничение на длину — чтобы уложиться в MAX_PATH даже во вложенных папках.
    let stem: String = stem.chars().take(80).collect();
    if stem.is_empty() {
        format!(
            "pda_chart_{}.{}",
            chrono::Local::now().format("%Y%m%d_%H%M%S"),
            ext
        )
    } else {
        format!("{stem}.{}", ext)
    }
}

/// Не перезаписывать прежний экспорт: chart.png, chart-1.png, chart-2.png...
/// Расширение сохраняется тем же, что у запрошенного имени (png, pdf, json, csv).
fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let path = std::path::Path::new(name);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_string();
    for i in 1..1000 {
        let candidate = if ext.is_empty() {
            dir.join(format!("{stem}-{i}"))
        } else {
            dir.join(format!("{stem}-{i}.{ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    first
}

/// Сохранить PNG графика на диск и вернуть путь.
///
/// Экспорт делает сервер, а не браузер: в WebView2 (оболочка pda-desktop)
/// скачивание по <a download> молча не доходит до пользователя, поэтому
/// интерфейс полагаться на него не может. Ответ содержит реальный путь,
/// который и показывается в сообщении.
pub async fn export_png(
    State(core): State<Arc<AppCore>>,
    Query(q): Query<ExportNameQuery>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    if body.len() < PNG_MAGIC.len() || body[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(ApiError(
            "Тело запроса не является PNG (пустое или повреждённое изображение)".into(),
        ));
    }
    if body.len() > MAX_PNG_BYTES {
        return Err(ApiError(format!(
            "Изображение слишком большое: {} МБ",
            body.len() / 1024 / 1024
        )));
    }
    let dir = core.exports_dir.clone();
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError(format!("Не удалось создать каталог {}: {e}", dir.display())))?;
    let path = unique_path(&dir, &safe_export_name(q.name.as_deref(), "png"));
    // Файл маленький и запись одноразовая по клику — блокирующий write здесь
    // уместнее, чем отдельная блокирующая задача (как в db_switch).
    std::fs::write(&path, &body)
        .map_err(|e| ApiError(format!("Не удалось записать {}: {e}", path.display())))?;
    tracing::info!(
        "PNG сохранён: {} ({} КБ)",
        path.display(),
        body.len() / 1024
    );
    Ok(Json(
        json!({ "ok": true, "path": path.to_string_lossy(), "bytes": body.len() }),
    ))
}

// ---------------------------------------------------------------- экспорт файлов

/// Разрешённые расширения текстовых выгрузок (дерево I/O, CSV переменных).
const TEXT_EXPORT_EXT: [&str; 3] = ["json", "csv", "txt"];

/// Имя файла для текстовой выгрузки: путь вырезается, недопустимые для файловой
/// системы символы отбрасываются (кириллица сохраняется — имена модулей у нас
/// русские), расширение — из белого списка (иначе .json по умолчанию).
fn safe_text_export_name(raw: Option<&str>) -> String {
    let base = raw
        .unwrap_or_default()
        .rsplit(SEPARATORS)
        .next()
        .unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ' '))
        .collect();
    let cleaned = cleaned.trim().replace(' ', "_");
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((s, e)) if TEXT_EXPORT_EXT.iter().any(|x| x.eq_ignore_ascii_case(e)) => {
            (s.to_string(), e.to_ascii_lowercase())
        }
        _ => (cleaned, "json".to_string()),
    };
    let stem: String = stem.trim_matches('.').chars().take(80).collect();
    if stem.is_empty() {
        format!(
            "pda_export_{}.{ext}",
            chrono::Local::now().format("%Y%m%d_%H%M%S")
        )
    } else {
        format!("{stem}.{ext}")
    }
}

/// Сохранить текстовую выгрузку (JSON-дерево I/O, CSV переменных) на диск и
/// вернуть путь. Как и PNG, файл пишет сервер: в WebView2 скачивание по
/// `<a download>` молча не доходит до пользователя.
pub async fn export_file(
    State(core): State<Arc<AppCore>>,
    Query(q): Query<ExportNameQuery>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    if body.len() > MAX_PNG_BYTES {
        return Err(ApiError(format!(
            "Файл слишком большой: {} МБ",
            body.len() / 1024 / 1024
        )));
    }
    let dir = core.exports_dir.clone();
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError(format!("Не удалось создать каталог {}: {e}", dir.display())))?;
    let path = unique_path(&dir, &safe_text_export_name(q.name.as_deref()));
    std::fs::write(&path, &body)
        .map_err(|e| ApiError(format!("Не удалось записать {}: {e}", path.display())))?;
    tracing::info!("Файл сохранён: {} ({} КБ)", path.display(), body.len() / 1024);
    Ok(Json(
        json!({ "ok": true, "path": path.to_string_lossy(), "bytes": body.len() }),
    ))
}

// ---------------------------------------------------------------- экспорт PDF

/// Цвет RGB из строки CSS: rgb(r, g, b) → (r/255, g/255, b/255).
fn css_rgb(s: &str) -> printpdf::Color {
    let nums: Vec<u8> = s
        .split(['(', ',', ')'])
        .filter_map(|n| n.trim().parse::<u8>().ok())
        .collect();
    if nums.len() >= 3 {
        printpdf::Color::Rgb(printpdf::Rgb::new(
            nums[0] as f32 / 255.0,
            nums[1] as f32 / 255.0,
            nums[2] as f32 / 255.0,
            None,
        ))
    } else {
        printpdf::Color::Rgb(printpdf::Rgb::new(0.0, 0.0, 0.0, None))
    }
}

/// Одна страница PDF: PNG графика + заголовок окна + легенда.
struct ChartPage {
    png: Vec<u8>,
    title: String,
    legend_names: Vec<String>,
    legend_colors: Vec<String>,
}

/// Разбор имени поля multipart: "chart_2" / "legend_name_2[]" -> ("chart", Some(2)),
/// "chart" -> ("chart", None). Индекс — номер окна графика на фронтенде.
fn split_field_index(name: &str) -> (&str, Option<usize>) {
    let trimmed = name.strip_suffix("[]").unwrap_or(name);
    match trimmed.rfind('_') {
        Some(pos) => match trimmed[pos + 1..].parse::<usize>() {
            Ok(i) => (&trimmed[..pos], Some(i)),
            Err(_) => (trimmed, None),
        },
        None => (trimmed, None),
    }
}

/// Экспорт PDF: клиент присылает PNG всех окон графиков + легенды.
/// Каждое окно — отдельная страница A4 landscape: график по центру,
/// под ним заголовок окна и легенда.
///
/// Поля multipart: `chart_{i}`, `title_{i}`, `legend_name_{i}[]`,
/// `legend_color_{i}[]`; `title` без индекса — общий заголовок (имя файла).
/// Все координаты в PDF: Pt (1pt = 1/72"), origin — левый НИЖНИЙ угол.
pub async fn export_pdf(
    State(core): State<Arc<AppCore>>,
    mut payload: axum::extract::Multipart,
) -> ApiResult<Json<Value>> {
    let dir = core.exports_dir.clone();
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError(format!("Не удалось создать каталог {}: {e}", dir.display())))?;

    // 1) Разбираем multipart в карту «номер окна -> страница».
    let mut pages: std::collections::BTreeMap<usize, ChartPage> = Default::default();
    let mut doc_title = String::new();

    while let Some(field) = payload
        .next_field()
        .await
        .map_err(|e| ApiError(e.to_string()))?
    {
        let raw_name = field.name().unwrap_or("").to_string();
        let bytes = field.bytes().await.map_err(|e| ApiError(e.to_string()))?;

        // Общий заголовок документа (он же имя файла) приходит без индекса.
        if raw_name == "title" {
            doc_title = String::from_utf8_lossy(&bytes).to_string();
            continue;
        }

        let (key, idx) = split_field_index(&raw_name);
        let page = pages.entry(idx.unwrap_or(0)).or_insert_with(|| ChartPage {
            png: Vec::new(),
            title: String::new(),
            legend_names: Vec::new(),
            legend_colors: Vec::new(),
        });
        match key {
            "chart" => page.png = bytes.to_vec(),
            "title" => page.title = String::from_utf8_lossy(&bytes).to_string(),
            "legend_name" => page
                .legend_names
                .push(String::from_utf8_lossy(&bytes).to_string()),
            "legend_color" => page
                .legend_colors
                .push(String::from_utf8_lossy(&bytes).to_string()),
            _ => {}
        }
    }

    if pages.is_empty() {
        return Err(ApiError("Не загружен ни один график".into()));
    }
    if doc_title.is_empty() {
        doc_title = "PDA".to_string();
    }

    // 2) Параметры страницы: A4 landscape 297×210 мм.
    let mm_to_pt = 2.834_646_f32;
    let page_w_mm = 297_f32;
    let page_h_mm = 210_f32;
    let page_w = page_w_mm * mm_to_pt;
    let page_h = page_h_mm * mm_to_pt;
    let margin = 15_f32 * mm_to_pt;

    let mut doc = printpdf::PdfDocument::new(&format!("PDA Next — {doc_title}"));

    // 3) Каждое окно графика — отдельная страница.
    for (idx, page) in pages.iter() {
        if page.png.len() < PNG_MAGIC.len() || page.png[..PNG_MAGIC.len()] != PNG_MAGIC {
            return Err(ApiError(format!("chart_{idx} не является PNG")));
        }
        let img_rgba = image::load_from_memory(&page.png)
            .map_err(|e| ApiError(format!("Не удалось декодировать PNG {idx}: {e}")))?
            .to_rgba8();
        let (img_w, img_h) = (img_rgba.width() as usize, img_rgba.height() as usize);

        // Высота легенды зависит от числа строк: заголовок окна + строки.
        let rows = page.legend_names.len().min(page.legend_colors.len());
        let legend_h = 18.0 + rows as f32 * 12.0 + 6.0;
        let chart_area_w = page_w - 2.0 * margin;
        let chart_area_h = (page_h - 2.0 * margin - legend_h).max(60.0);

        // Масштаб изображения (px → pt), с сохранением пропорций
        let scale = (chart_area_w / img_w as f32).min(chart_area_h / img_h as f32);
        let draw_x = margin + (chart_area_w - img_w as f32 * scale) / 2.0;
        // PDF origin — снизу; график сверху, легенда снизу
        let draw_y = margin + legend_h + (chart_area_h - img_h as f32 * scale) / 2.0;

        let raw = printpdf::RawImage {
            pixels: printpdf::RawImageData::U8(img_rgba.into_raw()),
            width: img_w,
            height: img_h,
            data_format: printpdf::RawImageFormat::RGBA8,
            tag: Vec::new(),
        };
        let img_id = doc.add_image(&raw);

        let mut ops: Vec<printpdf::Op> = Vec::new();

        // Белый фон страницы
        ops.push(printpdf::Op::SetFillColor {
            col: printpdf::Color::Rgb(printpdf::Rgb::new(1.0, 1.0, 1.0, None)),
        });
        ops.push(printpdf::Op::DrawRectangle {
            rectangle: printpdf::Rect {
                x: printpdf::Pt(0.0),
                y: printpdf::Pt(0.0),
                width: printpdf::Pt(page_w),
                height: printpdf::Pt(page_h),
                mode: None,
                winding_order: None,
            },
        });

        // График. ВАЖНО: printpdf 0.12 рисует изображение в квадрате 1×1 и
        // сам подставляет масштаб (px → pt по dpi) только при
        // no_auto_scale = false. С true картинка сжималась до ~1 pt —
        // страница выглядела пустой. dpi = 72 даёт 1 px = 1 pt, дальше
        // scale_x/y — наш вписывающий масштаб, translate — позиция.
        ops.push(printpdf::Op::UseXobject {
            id: img_id,
            transform: printpdf::XObjectTransform {
                translate_x: Some(printpdf::Pt(draw_x)),
                translate_y: Some(printpdf::Pt(draw_y)),
                rotate: None,
                scale_x: Some(scale),
                scale_y: Some(scale),
                dpi: Some(72.0),
                no_auto_scale: false,
            },
        });


        // 4) Легенда под графиком
        let mut ly = margin + legend_h - 8.0; // отсчёт сверху вниз

        // Название графика
        let page_title = if page.title.is_empty() {
            format!("График {}", idx + 1)
        } else {
            page.title.clone()
        };
        ops.push(printpdf::Op::StartTextSection);
        ops.push(printpdf::Op::SetFillColor {
            col: printpdf::Color::Rgb(printpdf::Rgb::new(0.1, 0.1, 0.1, None)),
        });
        ops.push(printpdf::Op::SetFont {
            font: printpdf::PdfFontHandle::Builtin(printpdf::BuiltinFont::HelveticaBold),
            size: printpdf::Pt(11.0),
        });
        ops.push(printpdf::Op::SetTextCursor {
            pos: printpdf::Point {
                x: printpdf::Pt(margin),
                y: printpdf::Pt(ly),
            },
        });
        ops.push(printpdf::Op::ShowText {
            items: vec![printpdf::TextItem::Text(page_title)],
        });
        ops.push(printpdf::Op::EndTextSection);

        ly -= 14.0;

        // Строки легенды
        for (name, color_str) in page.legend_names.iter().zip(page.legend_colors.iter()) {
            // Квадрат цвета
            ops.push(printpdf::Op::SetFillColor {
                col: css_rgb(color_str),
            });
            ops.push(printpdf::Op::DrawRectangle {
                rectangle: printpdf::Rect {
                    x: printpdf::Pt(margin),
                    y: printpdf::Pt(ly - 3.0),
                    width: printpdf::Pt(8.0),
                    height: printpdf::Pt(8.0),
                    mode: None,
                    winding_order: None,
                },
            });
            // Текст
            ops.push(printpdf::Op::StartTextSection);
            ops.push(printpdf::Op::SetFillColor {
                col: printpdf::Color::Rgb(printpdf::Rgb::new(0.1, 0.1, 0.1, None)),
            });
            ops.push(printpdf::Op::SetFont {
                font: printpdf::PdfFontHandle::Builtin(printpdf::BuiltinFont::Helvetica),
                size: printpdf::Pt(9.0),
            });
            ops.push(printpdf::Op::SetTextCursor {
                pos: printpdf::Point {
                    x: printpdf::Pt(margin + 11.0),
                    y: printpdf::Pt(ly),
                },
            });
            ops.push(printpdf::Op::ShowText {
                items: vec![printpdf::TextItem::Text(name.clone())],
            });
            ops.push(printpdf::Op::EndTextSection);
            ly -= 12.0;
        }

        // 5) Добавляем страницу
        doc.pages.push(printpdf::PdfPage::new(
            printpdf::Mm(page_w_mm),
            printpdf::Mm(page_h_mm),
            ops,
        ));
    } // конец цикла по окнам графиков

    // 6) Сохраняем
    let path = unique_path(&dir, &safe_export_name(Some(&doc_title), "pdf"));
    let mut warnings = Vec::new();
    let bytes = doc.save(&printpdf::PdfSaveOptions::default(), &mut warnings);
    std::fs::write(&path, &bytes)
        .map_err(|e| ApiError(format!("Не удалось записать {}: {e}", path.display())))?;
    tracing::info!(
        "PDF сохранён: {} ({} КБ)",
        path.display(),
        bytes.len() / 1024
    );
    Ok(Json(
        json!({ "ok": true, "path": path.to_string_lossy(), "bytes": bytes.len() }),
    ))
}

// ---------------------------------------------------------------- маршруты

pub fn router(core: Arc<AppCore>) -> Router {
    let api = Router::new()
        .route("/tags", get(tags).post(add_tag))
        .route("/tags/:id", put(update_tag))
        .route("/tags/delete", post(delete_tags))
        .route("/tags/move", post(move_tags))
        .route("/points/:tag_id", get(points))
        .route("/archive/:tag_id", get(archive))
        .route("/series", get(series))
        .route("/gaps", get(data_gaps))
        .route("/last", get(last_data))
        .route("/live", get(live))
        .route("/live/ws", get(crate::ws::live_ws))
        .route("/status", get(status))
        .route("/retention", post(set_retention))
        .route("/polling", post(set_polling))
        .route("/connections", get(connections).post(add_connection))
        .route(
            "/connections/:id",
            put(update_connection).delete(delete_connection),
        )
        .route("/connections/:id/restart", post(restart_connection))
        .route("/connections/:id/stop", post(stop_connection))
        .route("/connections/:id/start", post(start_connection))
        .route("/connections/:id/tags", get(tags_by_conn))
        .route("/connections/:id/groups", get(list_groups).post(add_group))
        .route("/connections/:id/groups/rename", post(rename_group))
        .route("/connections/:id/groups/delete", post(delete_group))
        .route("/drivers/info", get(driver_info))
        .route("/validate/address", post(validate_address))
        .route("/db/status", get(db_status))
        .route("/db/test", post(db_test))
        .route("/db/create", post(db_create))
        .route("/db/switch", post(db_switch))
        .route("/export/png", post(export_png))
        .route("/export/file", post(export_file))
        .route("/export/pdf", post(export_pdf))
        .with_state(core.clone())
        // Лимит тела по умолчанию в axum — 2 МБ, чемтёжный график в PNG может
        // быть больше. Свой порог (32 МБ) проверяет сам export_png.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_PNG_BYTES));

    // Статика фронтенда: web/ рядом с бинарником или rust_pda/web (dev).
    // api — уже Router<()> после with_state, поэтому внешний роутер состояния не требует.
    let web_root = crate::web_root();
    let static_router = tower_http::services::ServeDir::new(&web_root)
        .append_index_html_on_directories(false)
        .fallback(tower_http::services::ServeFile::new(format!(
            "{web_root}/index.html"
        )));

    Router::new()
        .nest("/api", api)
        .fallback_service(static_router)
}
