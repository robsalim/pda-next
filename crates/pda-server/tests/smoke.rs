//! Смоук-тест сервера: REST-контракт + сквозной цикл данных
//! (запись FC16 в modbus_server -> live реестр -> WebSocket -> история в SQLite,
//! затем modbus_client читает тот же HR160).
//!
//! Порт и файл БД — временные (bind :0, tempfile), поэтому тест не мешает
//! запущенному приложению.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio_modbus::prelude::*;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use pda_core::core::tag_registry::QUALITY_GOOD;
use pda_core::database::db_service::DbConfig;
use pda_server::{api, AppCore};

struct Harness {
    core: std::sync::Arc<AppCore>,
    base: String,
    ws: String,
    http: reqwest::Client,
    tmp: TempDir,
}

impl Harness {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let db_path = tmp.path().join("smoke.db").to_string_lossy().into_owned();
        let cfg = DbConfig { sqlite_path: db_path, ..Default::default() };

        let core = AppCore::from_config(cfg, tmp.path().join("db_config.json")).await;
        assert!(core.db_snapshot().await.is_available, "SQLite-база должна открыться");
        core.spawn_live_pump();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = api::router(core.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        Self { core, base: format!("http://{addr}"), ws: format!("ws://{addr}/api/live/ws"), http: reqwest::Client::new(), tmp }
    }

    async fn stop(&self) {
        self.core.manager.write().await.stop_all().await;
    }

    async fn get_json(&self, path: &str) -> Value {
        let r = self.http.get(format!("{}{}", self.base, path)).send().await.unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "GET {path} -> {status}: {v}");
        v
    }

    async fn post_json(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}{}", self.base, path)).json(&body).send().await.unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "POST {path} -> {status}: {v}");
        v
    }

    async fn live(&self) -> Vec<Value> {
        self.get_json("/api/live").await.as_array().unwrap().clone()
    }

    /// Ждёт, пока pred() станет истинной на live-снимках (поллинг /api/live).
    async fn wait_live(&self, timeout_ms: u64, mut pred: impl FnMut(&Vec<Value>) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let rows = self.live().await;
            if pred(&rows) {
                return rows;
            }
            assert!(Instant::now() < deadline, "wait_live: timeout, live={rows:?}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_points(&self, tag_id: i64, timeout_ms: u64) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let resp = self.get_json(&format!("/api/points/{tag_id}")).await;
            let values = resp["values"].as_array().unwrap().clone();
            if !values.is_empty() {
                return values;
            }
            assert!(Instant::now() < deadline, "wait_points: история не появилась для tag {tag_id}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Порт, который гарантированно свободен прямо сейчас (ОС выдаёт эфемерный).
async fn free_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_modbus_cycle() {
    let h = Harness::start().await;
    let port = free_port().await;

    // 1. Модули: сервер Modbus (эмуляция PLC) + клиент на тот же порт.
    h.post_json("/api/connections", json!({
        "name": "E2E SERVER", "driver_type": "modbus_server", "enabled": true,
        "poll_interval_ms": 200, "config": {"host": "127.0.0.1", "port": port}
    })).await;
    let cli = h.post_json("/api/connections", json!({
        "name": "E2E CLIENT", "driver_type": "modbus_client", "enabled": true,
        "poll_interval_ms": 200, "config": {"ip": "127.0.0.1", "port": port, "slave_id": 1}
    })).await;
    let client_id = cli["id"].as_i64().unwrap();

    // 2. Сигналы: FLOAT в HR160 и BOOL в C7 сервера; HR160 со scale=2,offset=1 клиента
    let t_float = h.post_json("/api/tags", json!({
        "id": 0, "connection_id": 1, "name": "e2e_float", "address_str": "HR160", "data_type": "FLOAT",
        "scale": 1.0, "offset_val": 0.0, "unit": "°C", "group_name": "Тест",
        "log_interval_ms": 1000, "deadband": 0.0
    })).await["id"].as_i64().unwrap();
    let t_bool = h.post_json("/api/tags", json!({
        "id": 0, "connection_id": 1, "name": "e2e_bool", "address_str": "C7", "data_type": "BOOL",
        "scale": 1.0, "offset_val": 0.0, "unit": "", "group_name": "Тест",
        "log_interval_ms": 1000, "deadband": 0.0
    })).await["id"].as_i64().unwrap();
    let t_cli = h.post_json("/api/tags", json!({
        "id": 0, "connection_id": client_id, "name": "cli_float", "address_str": "HR160", "data_type": "FLOAT",
        "scale": 2.0, "offset_val": 1.0, "unit": "", "group_name": "Тест",
        "log_interval_ms": 1000, "deadband": 0.0
    })).await["id"].as_i64().unwrap();

    // 3. Контракт /api/tags: id, name, address (строка), connection, group, unit
    let tags = h.get_json("/api/tags").await;
    let arr = tags.as_array().unwrap();
    let f = arr.iter().find(|t| t["id"] == json!(t_float)).unwrap();
    assert_eq!(f["address"], json!("HR160"), "адрес строкой, как в Python /api/tags");
    assert_eq!(f["connection"], json!("E2E SERVER"));
    assert_eq!(f["group"], json!("Тест"));
    assert_eq!(f["unit"], json!("°C"));

    // 4. Свежие теги без значений: quality = BAD
    let rows = h.live().await;
    let r = rows.iter().find(|x| x["id"] == json!(t_float)).expect("тег в live сразу после reload");
    assert_eq!(r["value"], Value::Null, "нет значений — value null");
    assert_ne!(r["quality"].as_i64().unwrap(), QUALITY_GOOD as i64, "качество BAD до первой записи");

    // 5. WS подключаем ДО записи, чтобы гарантированно поймать кадр live
    let (mut ws, _) = tokio_tungstenite::connect_async(&h.ws).await.unwrap();

    // 6. Сквозной цикл: внешний мастер пишет FC16 (FLOAT) и FC05 (BOOL) в сервер,
    //    сервер декодирует и публикует в реестр с меткой прихода посылки.
    let mut client = tokio_modbus::client::tcp::connect_slave(([127, 0, 0, 1], port).into(), Slave(1)).await.unwrap();
    let bytes = 50.75f32.to_bits().to_be_bytes();
    client
        .write_multiple_registers(160, &[u16::from_be_bytes([bytes[0], bytes[1]]), u16::from_be_bytes([bytes[2], bytes[3]])])
        .await
        .unwrap();
    client.write_single_coil(7, true).await.unwrap();

    // Тот же мастер должен прочитать записанное (self-check карт памяти)
    let regs = client.read_holding_registers(160, 2).await.unwrap();
    assert_eq!(regs[0], u16::from_be_bytes([bytes[0], bytes[1]]));
    let coils = client.read_coils(7, 1).await.unwrap();
    assert!(coils[0]);
    drop(client);

    // 7. Live реестра сервера показывает GOOD и корректные декодированные значения
    let rows = h.wait_live(10_000, |rows| {
        rows.iter().any(|r| r["id"] == json!(t_float) && r["value"].as_f64() == Some(50.75))
            && rows.iter().any(|r| r["id"] == json!(t_bool) && r["value"].as_f64() == Some(1.0))
    })
    .await;
    let r = rows.iter().find(|x| x["id"] == json!(t_float)).unwrap();
    assert_eq!(r["name"], json!("e2e_float"), "имя тега в live");
    assert_eq!(r["quality"].as_i64(), Some(QUALITY_GOOD as i64));

    // 8. WebSocket доставил live-кадр с тем же значением
    let mut got_ws = false;
    for _ in 0..40 {
        match tokio::time::timeout(Duration::from_millis(900), ws.next()).await {
            Ok(Some(Ok(WsMessage::Text(txt)))) => {
                let v: Value = serde_json::from_str(&txt).unwrap();
                if v["type"].as_str() == Some("live")
                    && v["rows"].as_array().unwrap().iter().any(|x| x["id"] == json!(t_float) && x["value"].as_f64() == Some(50.75))
                {
                    got_ws = true;
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(other) => panic!("unexpected ws frame: {other:?}"),
            Err(_) => break,
        }
    }
    assert!(got_ws, "WS доставил live-кадр со значением 50.75");

    // 9. История: журнал доносит значение до БД (виден через /api/points)
    let points = h.wait_points(t_float, 20_000).await;
    assert!(points.iter().any(|p| (p.as_f64().unwrap_or(0.0) - 50.75).abs() < 1e-6), "50.75 в истории: {points:?}");

    // 10. Клиентский модуль (создан вместе с сервером) читает тот же HR160:
    //     scale=2, offset=1 -> 102.5
    h.wait_live(15_000, |rows| {
        rows.iter().any(|r| r["id"] == json!(t_cli)
            && (r["value"].as_f64().unwrap_or(0.0) - 102.5).abs() < 1e-6
            && r["quality"].as_i64() == Some(QUALITY_GOOD as i64))
    })
    .await;

    // 11. /api/status: панель (app/drivers/running/db).
    // Сервер пишет "Running (Port ...)", клиент — "Connected"/"Polling · ...".
    let healthy = |d: &Value| {
        let s = d["status"].as_str().unwrap_or("");
        s.starts_with("Running") || s.starts_with("Connected") || s.starts_with("Polling")
    };
    let deadline = Instant::now() + Duration::from_millis(15_000);
    let st = loop {
        let st = h.get_json("/api/status").await;
        let drivers = st["drivers"].as_array().unwrap().clone();
        if drivers.len() == 2 && st["running"].as_u64() == Some(2) && drivers.iter().all(healthy) {
            break st;
        }
        assert!(Instant::now() < deadline, "драйверы не вышли в рабочее состояние: {drivers:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(st["app"]["name"].as_str(), Some(pda_core::config::APP_NAME));
    assert_eq!(st["db"]["available"].as_bool(), Some(true));

    // 12. Удаление сигналов -> reload -> точки исчезают из live
    h.post_json("/api/tags/delete", json!({ "ids": [t_float, t_bool, t_cli] })).await;
    let rows = h.live().await;
    assert!(rows.iter().all(|r| r["id"] != json!(t_float) && r["id"] != json!(t_cli)), "live пуст по удалённым: {rows:?}");

    // 13. Удаление модулей -> forget_connection
    for c in h.get_json("/api/connections").await.as_array().unwrap().clone() {
        let cid = c["id"].as_i64().unwrap();
        let r = h.http.delete(format!("{}/api/connections/{}", h.base, cid)).send().await.unwrap();
        assert!(r.status().is_success(), "DELETE connection {cid} -> {}", r.status());
    }
    assert!(h.live().await.is_empty(), "реестр пуст после удаления модулей");

    h.stop().await;
    let _ = h.tmp.path();
}

#[tokio::test(flavor = "multi_thread")]
async fn helpers_and_validation_endpoints() {
    let h = Harness::start().await;

    // /api/drivers/info: ключи и подписи типов (конструктор IO-вкладки)
    let info = h.get_json("/api/drivers/info").await;
    let types = info["types"].as_array().unwrap();
    assert!(types.len() >= 3, "реестр драйверов: {types:?}");
    for t in types {
        assert!(t["key"].is_string() && t["label"].is_string(), "контракт drivers/types: {t}");
        assert!(t["address"].is_object(), "справка по адресам у каждого типа: {t}");
    }

    // /api/validate/address: успех и ошибка
    let ok = h.post_json("/api/validate/address", json!({ "driver_type": "modbus_client", "address": "HR100", "data_type": "UINT16" })).await;
    assert_eq!(ok["ok"], json!(true), "{ok}");
    let bad = h.post_json("/api/validate/address", json!({ "driver_type": "modbus_client", "address": "DB1.DBD0", "data_type": "FLOAT" })).await;
    assert_eq!(bad["ok"], json!(false), "s7-адрес не должен проходить modbus-валидатор");
    let wrong_type = h.post_json("/api/validate/address", json!({ "driver_type": "modbus_client", "address": "C7", "data_type": "INT16" })).await;
    assert_eq!(wrong_type["ok"], json!(false), "INT16 в катушку — ошибка");

    // /api/db/status: sqlite по умолчанию
    let db = h.get_json("/api/db/status").await;
    assert_eq!(db["available"], json!(true));
    assert_eq!(db["config"]["engine"], json!("sqlite"));

    // /api/retention
    let r = h.post_json("/api/retention", json!({ "days": 7.5 })).await;
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["days"].as_f64(), Some(7.5));

    // /api/db/test на мёртвый postgres: ok=false + понятная ошибка (не 500)
    let dead = free_port().await;
    let t = h.post_json("/api/db/test", json!({
        "engine": "postgres", "host": "127.0.0.1", "port": dead,
        "dbname": "none", "user": "none", "password": "none"
    })).await;
    assert_eq!(t["ok"], json!(false), "тест честно сообщает об ошибке: {t:?}");
    assert!(t["error"].is_string(), "текст ошибки обязателен: {t}");

    // /api/db/test на свою sqlite-файл: успех
    let ok_sqlite = h.post_json("/api/db/test", json!({
        "engine": "sqlite", "sqlite_path": h.tmp.path().join("smoke.db").to_string_lossy()
    })).await;
    assert_eq!(ok_sqlite["ok"], json!(true), "{ok_sqlite}");

    // /api/db/create: sqlite — «создаётся автоматически», не сетевой движок — ошибка
    let c = h.post_json("/api/db/create", json!({ "engine": "sqlite", "sqlite_path": "x.db" })).await;
    assert_eq!(c["ok"], json!(true), "sqlite create — информационный ответ: {c}");

    h.stop().await;
    let _ = h.tmp.path();
}

/// Единый frontend раздаётся тем же сервером: index + все модули скриптов.
#[tokio::test(flavor = "multi_thread")]
async fn frontend_assets_served() {
    let h = Harness::start().await;
    for (path, marker) in [
        ("/", "btn-tab-charts"),         // index.html — вкладки
        ("/app.css", "--plot-bg"),       // CSS с токенами темы
        ("/app.js", "new uPlot"),        // Charts/Bits
        ("/io.js", "ioRefreshAll"),      // вкладка I/O
        ("/db.js", "dbRefreshPanel"),    // вкладка Database
        ("/help.js", "HELP_SECTIONS"),   // вкладка Help
    ] {
        let r = h.http.get(format!("{}{}", h.base, path)).send().await.unwrap();
        assert!(r.status().is_success(), "GET {path} -> {}", r.status());
        let text = r.text().await.unwrap();
        assert!(text.contains(marker), "{path} должен содержать '{marker}'");
    }
    h.stop().await;
    let _ = h.tmp.path();
}
