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

use pda_core::database::db_service::{DbConfig, DbConfigStore, DatabaseService};
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

/// Разбор текстового окна ("15 мин", "1 час", ...) — копия parse_span из
/// web_server.py, порядок проверок сохранён один в один.
pub fn parse_span_ms(text: &str) -> i64 {
    let t = text.trim();
    if t.contains("15") && t.contains("мин") {
        return 15 * 60_000;
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
            let ts = p.timestamp.and_local_timezone(Local).single().map(|d| d.timestamp() as f64).unwrap_or(0.0);
            (ts, p.value)
        })
        .collect()
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
    let name_of = |cid: i64| conns.iter().find(|c| c.id == cid).map(|c| c.name.clone()).unwrap_or_default();
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
pub async fn points(
    State(core): State<Arc<AppCore>>,
    UrlPath(tag_id): UrlPath<i64>,
    Query(q): Query<SpanQuery>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let delta_ms = parse_span_ms(&q.span);
    let (points, t_start, t_end) = db.get_data_points_by_span(tag_id, delta_ms).await?;
    let (times, values): (Vec<f64>, Vec<f64>) = points_payload(points).into_iter().unzip();
    Ok(Json(json!({ "times": times, "values": values, "t_start": t_start, "t_end": t_end })))
}

#[derive(serde::Deserialize)]
pub struct ArchiveQuery {
    start: String,
    #[serde(default = "default_span")]
    duration: String,
}

/// Исторический диапазон (Python /api/archive).
pub async fn archive(
    State(core): State<Arc<AppCore>>,
    UrlPath(tag_id): UrlPath<i64>,
    Query(q): Query<ArchiveQuery>,
) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let start_dt = parse_iso_local(&q.start).unwrap_or_else(|| Local::now() - chrono::Duration::hours(1));
    let dur_ms = parse_span_ms(&q.duration);
    let end_dt = start_dt + chrono::Duration::milliseconds(dur_ms);
    let points = db.get_data_points_range(tag_id, start_dt, end_dt).await?;
    let (times, values): (Vec<f64>, Vec<f64>) = points_payload(points).into_iter().unzip();
    Ok(Json(json!({
        "times": times,
        "values": values,
        "t_start": start_dt.timestamp() as f64,
        "t_end": end_dt.timestamp() as f64,
    })))
}

/// Текущие значения из реестра (Python /api/live), с именами для таблиц IO.
pub async fn live(State(core): State<Arc<AppCore>>) -> Json<Value> {
    let rows: Vec<Value> = core
        .registry
        .live_snapshot()
        .into_iter()
        .map(|lv| {
            let name = core.registry.get(lv.tag_id).map(|p| p.name()).unwrap_or_default();
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
    Json(json!({
        "app": { "name": pda_core::config::APP_NAME, "version": pda_core::config::APP_VERSION },
        "drivers": mgr.drivers_json(),
        "running": mgr.running_count(),
        "journal": mgr.journal_stats(),
        "db": {
            "available": db.is_available,
            "engine": format!("{:?}", db.config().engine).to_lowercase(),
            "error": db.last_error.clone(),
            "config": db.config().to_json(),
        },
    }))
}

#[derive(serde::Deserialize)]
pub struct RetentionBody {
    pub days: f64,
}

pub async fn set_retention(State(core): State<Arc<AppCore>>, Json(b): Json<RetentionBody>) -> Json<Value> {
    let mgr = core.manager.read().await;
    mgr.set_retention_days(b.days);
    Json(json!({ "ok": true, "days": b.days }))
}

// ---------------------------------------------------------------- IO: подключения

pub async fn connections(State(core): State<Arc<AppCore>>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let conns = db.get_all_connections().await?;
    Ok(Json(serde_json::to_value(&conns).map_err(|e| ApiError(e.to_string()))?))
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
    Connection::new(id, b.name, b.driver_type, b.enabled, b.poll_interval_ms, config)
}

pub async fn add_connection(State(core): State<Arc<AppCore>>, Json(b): Json<ConnBody>) -> ApiResult<Json<Value>> {
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

pub async fn delete_connection(State(core): State<Arc<AppCore>>, UrlPath(id): UrlPath<i64>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.delete_connection(id).await?;
    {
        let mut mgr = core.manager.write().await;
        mgr.restart_all().await;
        // Реестр не должен хранить точки удалённого модуля даже после сбоя запуска.
        mgr.registry.forget_connection(id);
    }
    Ok(Json(json!({ "ok": true })))
}

/// Перезапуск одного модуля (Python: «Перезапустить» в контекстном меню).
pub async fn restart_connection(State(core): State<Arc<AppCore>>, UrlPath(id): UrlPath<i64>) -> ApiResult<Json<Value>> {
    core.manager.write().await.restart_connection(id).await;
    Ok(Json(json!({ "ok": true })))
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

pub async fn list_groups(State(core): State<Arc<AppCore>>, UrlPath(id): UrlPath<i64>) -> ApiResult<Json<Value>> {
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

pub async fn tags_by_conn(State(core): State<Arc<AppCore>>, UrlPath(id): UrlPath<i64>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    let tags = db.get_tags_by_connection(id).await?;
    Ok(Json(serde_json::to_value(&tags).map_err(|e| ApiError(e.to_string()))?))
}

pub async fn add_tag(State(core): State<Arc<AppCore>>, Json(tag): Json<Tag>) -> ApiResult<Json<Value>> {
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

pub async fn delete_tags(State(core): State<Arc<AppCore>>, Json(b): Json<IdsBody>) -> ApiResult<Json<Value>> {
    let db = core.db_snapshot().await;
    db.delete_tags(&b.ids).await?;
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
    let err = pda_core::drivers::registry::validate_address(&b.driver_type, &b.address, &b.data_type);
    Json(json!({ "ok": err.is_none(), "error": err }))
}

// ---------------------------------------------------------------- База данных

pub async fn db_status(State(core): State<Arc<AppCore>>) -> Json<Value> {
    let db = core.db_snapshot().await;
    Json(json!({
        "available": db.is_available,
        "error": db.last_error.clone(),
        "config": db.config().to_json(),
    }))
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
            let n = candidate.get_all_connections().await.map(|c| c.len()).unwrap_or(0);
            Json(json!({ "ok": true, "connections": n }))
        }
    }
}

/// Создать целевую базу (аналог Python create_database). SQLite создаётся
/// автоматически при применении; MariaDB/MySQL пока не перенесены.
pub async fn db_create(Json(b): Json<DbSwitchBody>) -> ApiResult<Json<Value>> {
    match b.engine.as_str() {
        "sqlite" => Ok(Json(json!({ "ok": true, "created": false, "info": "Для SQLite база создаётся автоматически при применении подключения." }))),
        "postgres" => {
            let cfg = b.into_cfg();
            match DatabaseService::create_postgres_database(&cfg).await {
                Ok(created) => Ok(Json(json!({ "ok": true, "created": created }))),
                Err(e) => Err(ApiError(e)),
            }
        }
        _ => Err(ApiError("Создание базы через GUI доступно только для PostgreSQL".into())),
    }
}

/// Применить и переключить (Python _save_and_reconnect): пишем db_config.json,
/// останавливаем драйверы, открываем новую базу, перезапускаем.
pub async fn db_switch(State(core): State<Arc<AppCore>>, Json(b): Json<DbSwitchBody>) -> ApiResult<Json<Value>> {
    let body_cfg = b.to_json();
    let cfg = b.into_cfg();
    core.switch_database(cfg).await.map_err(ApiError)?;
    if let Err(e) = std::fs::write(&core.db_config_path, serde_json::to_string_pretty(&body_cfg).unwrap()) {
        tracing::warn!("Не удалось сохранить {}: {e}", core.db_config_path.display());
    }
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- маршруты

pub fn router(core: Arc<AppCore>) -> Router {
    let api = Router::new()
        .route("/tags", get(tags).post(add_tag))
        .route("/tags/:id", put(update_tag))
        .route("/tags/delete", post(delete_tags))
        .route("/points/:tag_id", get(points))
        .route("/archive/:tag_id", get(archive))
        .route("/live", get(live))
        .route("/live/ws", get(crate::ws::live_ws))
        .route("/status", get(status))
        .route("/retention", post(set_retention))
        .route("/connections", get(connections).post(add_connection))
        .route("/connections/:id", put(update_connection).delete(delete_connection))
        .route("/connections/:id/restart", post(restart_connection))
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
        .with_state(core.clone());

    // Статика фронтенда: web/ рядом с бинарником или rust_pda/web (dev).
    // api — уже Router<()> после with_state, поэтому внешний роутер состояния не требует.
    let web_root = crate::web_root();
    let static_router = tower_http::services::ServeDir::new(&web_root)
        .append_index_html_on_directories(false)
        .fallback(tower_http::services::ServeFile::new(format!("{web_root}/index.html")));

    Router::new().nest("/api", api).fallback_service(static_router)
}
