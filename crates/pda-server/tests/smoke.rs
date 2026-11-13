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
use pda_core::database::db_service::{DatabaseService, DbConfig};
use pda_server::{api, AppCore};

struct Harness {
    core: std::sync::Arc<AppCore>,
    base: String,
    ws: String,
    http: reqwest::Client,
    tmp: TempDir,
    /// Каталог экспорта PNG — временный, чтобы тест не писал в «Загрузки».
    exports: std::path::PathBuf,
}

impl Harness {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("tmp dir");
        let db_path = tmp.path().join("smoke.db").to_string_lossy().into_owned();
        let cfg = DbConfig {
            sqlite_path: db_path,
            ..Default::default()
        };

        let exports = tmp.path().join("exports");
        let db = std::sync::Arc::new(DatabaseService::open(cfg).await);
        let core = AppCore::with_config_and_exports(
            db,
            tmp.path().join("db_config.json"),
            exports.clone(),
        );
        assert!(
            core.db_snapshot().await.is_available,
            "SQLite-база должна открыться"
        );
        core.spawn_live_pump();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = api::router(core.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        Self {
            core,
            base: format!("http://{addr}"),
            ws: format!("ws://{addr}/api/live/ws"),
            http: reqwest::Client::new(),
            tmp,
            exports,
        }
    }

    async fn stop(&self) {
        self.core.manager.write().await.stop_all().await;
    }

    async fn get_json(&self, path: &str) -> Value {
        let r = self
            .http
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "GET {path} -> {status}: {v}");
        v
    }

    async fn post_json(&self, path: &str, body: Value) -> Value {
        let r = self
            .http
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(status.is_success(), "POST {path} -> {status}: {v}");
        v
    }

    async fn live(&self) -> Vec<Value> {
        self.get_json("/api/live").await.as_array().unwrap().clone()
    }

    /// Ждёт, пока pred() станет истинной на live-снимках (поллинг /api/live).
    async fn wait_live(
        &self,
        timeout_ms: u64,
        mut pred: impl FnMut(&Vec<Value>) -> bool,
    ) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let rows = self.live().await;
            if pred(&rows) {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "wait_live: timeout, live={rows:?}"
            );
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
            assert!(
                Instant::now() < deadline,
                "wait_points: история не появилась для tag {tag_id}"
            );
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
    h.post_json(
        "/api/connections",
        json!({
            "name": "E2E SERVER", "driver_type": "modbus_server", "enabled": true,
            "poll_interval_ms": 200, "config": {"host": "127.0.0.1", "port": port}
        }),
    )
    .await;
    let cli = h
        .post_json(
            "/api/connections",
            json!({
                "name": "E2E CLIENT", "driver_type": "modbus_client", "enabled": true,
                "poll_interval_ms": 200, "config": {"ip": "127.0.0.1", "port": port, "slave_id": 1}
            }),
        )
        .await;
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
    assert_eq!(
        f["address"],
        json!("HR160"),
        "адрес строкой, как в Python /api/tags"
    );
    assert_eq!(f["connection"], json!("E2E SERVER"));
    assert_eq!(f["group"], json!("Тест"));
    assert_eq!(f["unit"], json!("°C"));

    // 4. Свежие теги без значений: quality = BAD
    let rows = h.live().await;
    let r = rows
        .iter()
        .find(|x| x["id"] == json!(t_float))
        .expect("тег в live сразу после reload");
    assert_eq!(r["value"], Value::Null, "нет значений — value null");
    assert_ne!(
        r["quality"].as_i64().unwrap(),
        QUALITY_GOOD as i64,
        "качество BAD до первой записи"
    );

    // 5. WS подключаем ДО записи, чтобы гарантированно поймать кадр live
    let (mut ws, _) = tokio_tungstenite::connect_async(&h.ws).await.unwrap();

    // 6. Сквозной цикл: внешний мастер пишет FC16 (FLOAT) и FC05 (BOOL) в сервер,
    //    сервер декодирует и публикует в реестр с меткой прихода посылки.
    let mut client =
        tokio_modbus::client::tcp::connect_slave(([127, 0, 0, 1], port).into(), Slave(1))
            .await
            .unwrap();
    let bytes = 50.75f32.to_bits().to_be_bytes();
    client
        .write_multiple_registers(
            160,
            &[
                u16::from_be_bytes([bytes[0], bytes[1]]),
                u16::from_be_bytes([bytes[2], bytes[3]]),
            ],
        )
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
    let rows = h
        .wait_live(10_000, |rows| {
            rows.iter()
                .any(|r| r["id"] == json!(t_float) && r["value"].as_f64() == Some(50.75))
                && rows
                    .iter()
                    .any(|r| r["id"] == json!(t_bool) && r["value"].as_f64() == Some(1.0))
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
                    && v["rows"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|x| x["id"] == json!(t_float) && x["value"].as_f64() == Some(50.75))
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
    assert!(
        points
            .iter()
            .any(|p| (p.as_f64().unwrap_or(0.0) - 50.75).abs() < 1e-6),
        "50.75 в истории: {points:?}"
    );

    // 10. Клиентский модуль (создан вместе с сервером) читает тот же HR160:
    //     scale=2, offset=1 -> 102.5
    h.wait_live(15_000, |rows| {
        rows.iter().any(|r| {
            r["id"] == json!(t_cli)
                && (r["value"].as_f64().unwrap_or(0.0) - 102.5).abs() < 1e-6
                && r["quality"].as_i64() == Some(QUALITY_GOOD as i64)
        })
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
        assert!(
            Instant::now() < deadline,
            "драйверы не вышли в рабочее состояние: {drivers:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(st["app"]["name"].as_str(), Some(pda_core::config::APP_NAME));
    assert_eq!(st["db"]["available"].as_bool(), Some(true));

    // 12. Удаление сигналов -> reload -> точки исчезают из live
    h.post_json(
        "/api/tags/delete",
        json!({ "ids": [t_float, t_bool, t_cli] }),
    )
    .await;
    let rows = h.live().await;
    assert!(
        rows.iter()
            .all(|r| r["id"] != json!(t_float) && r["id"] != json!(t_cli)),
        "live пуст по удалённым: {rows:?}"
    );

    // 13. Удаление модулей -> forget_connection
    for c in h
        .get_json("/api/connections")
        .await
        .as_array()
        .unwrap()
        .clone()
    {
        let cid = c["id"].as_i64().unwrap();
        let r = h
            .http
            .delete(format!("{}/api/connections/{}", h.base, cid))
            .send()
            .await
            .unwrap();
        assert!(
            r.status().is_success(),
            "DELETE connection {cid} -> {}",
            r.status()
        );
    }
    assert!(
        h.live().await.is_empty(),
        "реестр пуст после удаления модулей"
    );

    h.stop().await;
    let _ = h.tmp.path();
}

/// running_count в /api/status меняется асинхронно: задача драйвера поднимает
/// флаг после bind. Тест ждёт целевое количество, а не читает сразу.
impl Harness {
    async fn wait_running(&self, want: i64) -> Value {
        let mut st = json!(null);
        for _ in 0..50 {
            st = self.get_json("/api/status").await;
            if st["running"].as_i64() == Some(want) {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        panic!("running_count не стал {want}: {st:?}");
    }
}

/// Точечный стоп/старт одного модуля через REST: остановленный не опрашивает,
/// но запускается обратно; /api/status показывает его в manually_stopped.
#[tokio::test(flavor = "multi_thread")]
async fn stop_start_single_connection() {
    let h = Harness::start().await;
    let port = free_port().await;
    let srv = h.post_json(
        "/api/connections",
        json!({
            "name": "STOP TEST", "driver_type": "modbus_server", "enabled": true,
            "poll_interval_ms": 200, "config": {"host": "127.0.0.1", "port": port}
        }),
    ).await;
    let sid = srv["id"].as_i64().unwrap();
    h.post_json("/api/tags", json!({
        "id": 0, "connection_id": sid, "name": "st_float", "address_str": "HR160", "data_type": "FLOAT",
        "scale": 1.0, "offset_val": 0.0, "unit": "", "group_name": "Тест",
        "log_interval_ms": 1000, "deadband": 0.0
    })).await;

    // Драйвер поднялся (add_connection вызывает restart_all)
    let st = h.wait_running(1).await;
    assert!(st["manually_stopped"].as_array().unwrap().is_empty());

    // Стоп — running=0, пауза в списке
    let r = h.post_json(&format!("/api/connections/{sid}/stop"), json!({})).await;
    assert_eq!(r["ok"], json!(true));
    let st = h.wait_running(0).await;
    assert!(st["manually_stopped"].as_array().unwrap().contains(&json!(sid)),
        "модуль числится остановленным вручную: {st:?}");

    // Старт обратно
    let r = h.post_json(&format!("/api/connections/{sid}/start"), json!({})).await;
    assert_eq!(r["ok"], json!(true));
    let st = h.wait_running(1).await;
    assert!(st["manually_stopped"].as_array().unwrap().is_empty(),
        "после start паузы нет: {st:?}");

    h.stop().await;
    let _ = h.tmp.path();
}

/// Ручная пауза переживает «рестарт процесса»: второй AppCore поверх той же
/// БД и того же pda_paused.json (рядом с db_config.json) не поднимает
/// остановленный модуль при start(), а start_connection снимает паузу.
#[tokio::test(flavor = "multi_thread")]
async fn manual_pause_survives_process_restart() {
    let h = Harness::start().await;
    let port = free_port().await;
    let srv = h.post_json(
        "/api/connections",
        json!({
            "name": "PAUSE RESTART", "driver_type": "modbus_server", "enabled": true,
            "poll_interval_ms": 200, "config": {"host": "127.0.0.1", "port": port}
        }),
    ).await;
    let sid = srv["id"].as_i64().unwrap();
    h.wait_running(1).await;
    let _ = h.post_json(&format!("/api/connections/{sid}/stop"), json!({})).await;
    h.wait_running(0).await;

    // Имитация перезапуска процесса: новый AppCore поверх тех же файлов.
    let db_path = h.tmp.path().join("smoke.db").to_string_lossy().into_owned();
    h.stop().await;
    let cfg = DbConfig { sqlite_path: db_path, ..Default::default() };
    let db2 = std::sync::Arc::new(DatabaseService::open(cfg).await);
    assert!(db2.is_available, "та же БД открыта заново");
    let core2 = AppCore::with_config_path(db2, h.tmp.path().join("db_config.json"));
    core2.start(false).await;

    {
        let mgr = core2.manager.read().await;
        assert!(mgr.is_manually_stopped(sid), "пауза загружена при старте ядра");
        assert_eq!(mgr.running_count(), 0, "start_all не поднимает ручную паузу");
    }

    // Снятие паузы работает и после рестарта.
    assert!(core2.manager.write().await.start_connection(sid).await);
    for _ in 0..50 {
        if core2.manager.read().await.running_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(core2.manager.read().await.running_count(), 1, "модуль снова опрашивается");
    core2.manager.write().await.stop_all().await;
    h.stop().await;
    let _ = h.tmp.path();
}

/// «⏸ Опрос» (view_only) должен останавливать и журнал: heartbeat-теги
/// (log_interval_ms) иначе продолжают писать каждую секунду в стоящий опрос —
/// счётчик «записано» растёт, база наполняется дублями при пустом графике.
#[tokio::test(flavor = "multi_thread")]
async fn view_only_stops_journal_writes() {
    let h = Harness::start().await;
    let port = free_port().await;
    h.post_json(
        "/api/connections",
        json!({
            "name": "VO SRV", "driver_type": "modbus_server", "enabled": true,
            "poll_interval_ms": 200, "config": {"host": "127.0.0.1", "port": port}
        }),
    )
    .await;
    h.wait_running(1).await;
    h.post_json(
        "/api/tags",
        json!({
            "id": 0, "connection_id": 1, "name": "vo_float", "address_str": "HR160",
            "data_type": "FLOAT", "scale": 1.0, "offset_val": 0.0, "unit": "",
            "group_name": "Тест", "log_interval_ms": 1000, "deadband": 0.0
        }),
    )
    .await;

    let mut client = tokio_modbus::client::tcp::connect_slave(([127, 0, 0, 1], port).into(), Slave(1))
        .await
        .unwrap();
    let bytes = 42.5f32.to_bits().to_be_bytes();
    client
        .write_multiple_registers(
            160,
            &[
                u16::from_be_bytes([bytes[0], bytes[1]]),
                u16::from_be_bytes([bytes[2], bytes[3]]),
            ],
        )
        .await
        .unwrap();
    drop(client);

    // При включённом опросе журнал пишет (первая точка + heartbeat раз в секунду).
    let written = |s: &Value| s["journal"]["written"].as_i64().unwrap_or(0);
    let w0 = written(&h.get_json("/api/status").await);
    let mut w1 = w0;
    for _ in 0..60 {
        w1 = written(&h.get_json("/api/status").await);
        if w1 > w0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(w1 > w0, "при опросе журнал пишет: written={w1}");

    // «⏸ Опрос»: драйверы И журнал останавливаются — записи больше нет.
    let p = h.post_json("/api/polling", json!({ "enabled": false })).await;
    assert_eq!(p["view_only"], json!(true));
    tokio::time::sleep(Duration::from_millis(300)).await; // добить финальный flush
    let a = written(&h.get_json("/api/status").await);
    tokio::time::sleep(Duration::from_millis(1400)).await // больше heartbeat-периода
    ;
    let b = written(&h.get_json("/api/status").await);
    assert_eq!(a, b, "в режиме только просмотр журнал не пишет (было {a}, стало {b})");

    // Возврат к опросу — журнал снова работает.
    h.post_json("/api/polling", json!({ "enabled": true })).await;
    h.wait_running(1).await;
    let mut client = tokio_modbus::client::tcp::connect_slave(([127, 0, 0, 1], port).into(), Slave(1))
        .await
        .unwrap();
    let bytes2 = 43.5f32.to_bits().to_be_bytes();
    client
        .write_multiple_registers(
            160,
            &[
                u16::from_be_bytes([bytes2[0], bytes2[1]]),
                u16::from_be_bytes([bytes2[2], bytes2[3]]),
            ],
        )
        .await
        .unwrap();
    drop(client);
    let mut ok = false;
    for _ in 0..60 {
        if written(&h.get_json("/api/status").await) > b {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "после включения опроса журнал возобновил запись");
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
        assert!(
            t["key"].is_string() && t["label"].is_string(),
            "контракт drivers/types: {t}"
        );
        assert!(
            t["address"].is_object(),
            "справка по адресам у каждого типа: {t}"
        );
    }

    // /api/validate/address: успех и ошибка
    let ok = h
        .post_json(
            "/api/validate/address",
            json!({ "driver_type": "modbus_client", "address": "HR100", "data_type": "UINT16" }),
        )
        .await;
    assert_eq!(ok["ok"], json!(true), "{ok}");
    let bad = h
        .post_json(
            "/api/validate/address",
            json!({ "driver_type": "modbus_client", "address": "DB1.DBD0", "data_type": "FLOAT" }),
        )
        .await;
    assert_eq!(
        bad["ok"],
        json!(false),
        "s7-адрес не должен проходить modbus-валидатор"
    );
    let wrong_type = h
        .post_json(
            "/api/validate/address",
            json!({ "driver_type": "modbus_client", "address": "C7", "data_type": "INT16" }),
        )
        .await;
    assert_eq!(wrong_type["ok"], json!(false), "INT16 в катушку — ошибка");

    // /api/db/status: sqlite по умолчанию
    let db = h.get_json("/api/db/status").await;
    assert_eq!(db["available"], json!(true));
    assert_eq!(db["config"]["engine"], json!("sqlite"));

    // /api/retention
    // Предписываем db_config.json так, как это сделал бы первый запуск в проде
    // (путь к той же временной базе) — дальше /api/retention должен пропатчить файл.
    std::fs::write(
        h.tmp.path().join("db_config.json"),
        serde_json::to_string_pretty(&json!({
            "engine": "sqlite",
            "sqlite_path": h.tmp.path().join("smoke.db").to_string_lossy(),
            "retention_days": 0.0,
        }))
        .unwrap(),
    )
    .unwrap();
    let r = h.post_json("/api/retention", json!({ "days": 7.5 })).await;
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["days"].as_f64(), Some(7.5));

    // retention персистится в db_config.json и восстанавливается при from_config
    // (имитация рестарта процесса).
    let file = h.tmp.path().join("db_config.json");
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(&file).expect("db_config.json записан"),
    )
    .expect("db_config.json — валидный JSON");
    assert_eq!(saved["retention_days"], json!(7.5));
    let restored = AppCore::from_config(DbConfig::from_json(&saved), file.clone()).await;
    assert_eq!(
        restored.manager.read().await.journal_stats()["retention_days"],
        json!(7.5),
        "глубина хранения должна пережить рестарт"
    );
    restored.manager.write().await.stop_all().await;

    // /api/db/test на мёртвый postgres: ok=false + понятная ошибка (не 500)
    let dead = free_port().await;
    let t = h
        .post_json(
            "/api/db/test",
            json!({
                "engine": "postgres", "host": "127.0.0.1", "port": dead,
                "dbname": "none", "user": "none", "password": "none"
            }),
        )
        .await;
    assert_eq!(
        t["ok"],
        json!(false),
        "тест честно сообщает об ошибке: {t:?}"
    );
    assert!(t["error"].is_string(), "текст ошибки обязателен: {t}");

    // /api/db/test на свою sqlite-файл: успех
    let ok_sqlite = h
        .post_json(
            "/api/db/test",
            json!({
                "engine": "sqlite", "sqlite_path": h.tmp.path().join("smoke.db").to_string_lossy()
            }),
        )
        .await;
    assert_eq!(ok_sqlite["ok"], json!(true), "{ok_sqlite}");

    // /api/db/create: sqlite — «создаётся автоматически», не сетевой движок — ошибка
    let c = h
        .post_json(
            "/api/db/create",
            json!({ "engine": "sqlite", "sqlite_path": "x.db" }),
        )
        .await;
    assert_eq!(
        c["ok"],
        json!(true),
        "sqlite create — информационный ответ: {c}"
    );

    h.stop().await;
    let _ = h.tmp.path();
}

/// Единый frontend раздаётся тем же сервером: index + все модули скриптов.
#[tokio::test(flavor = "multi_thread")]
async fn frontend_assets_served() {
    let h = Harness::start().await;
    for (path, marker) in [
        ("/", "btn-tab-charts"),       // index.html — вкладки
        ("/m.html", "Сигнал не выбран"), // мобильная мини-версия (один график)
        ("/app.css", "--plot-bg"),     // CSS с токенами темы
        ("/app.js", "new uPlot"),      // Charts/Bits
        ("/io.js", "ioRefreshAll"),    // вкладка I/O
        ("/db.js", "dbRefreshPanel"),  // вкладка Database
        ("/help.js", "HELP_SECTIONS"), // вкладка Help
    ] {
        let r = h
            .http
            .get(format!("{}{}", h.base, path))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "GET {path} -> {}", r.status());
        let text = r.text().await.unwrap();
        assert!(text.contains(marker), "{path} должен содержать '{marker}'");
    }
    h.stop().await;
    let _ = h.tmp.path();
}

/// parse_span_ms: все подписи окон из комбобоксов десктопа,
/// «15 мин» не должен разбираться как «5 мин».
#[test]
fn parse_span_ms_spans() {
    assert_eq!(api::parse_span_ms("5 мин"), 5 * 60_000);
    assert_eq!(api::parse_span_ms("15 мин"), 15 * 60_000);
    assert_eq!(api::parse_span_ms("1 час"), 3_600_000);
    assert_eq!(api::parse_span_ms("4 часа"), 4 * 3_600_000);
    assert_eq!(api::parse_span_ms("12 часов"), 12 * 3_600_000);
    assert_eq!(api::parse_span_ms("24 часа"), 24 * 3_600_000);
    assert_eq!(api::parse_span_ms("72 часа"), 72 * 3_600_000);
    assert_eq!(
        api::parse_span_ms("что-то другое"),
        3_600_000,
        "дефолт 1 час, как в Python"
    );
}

/// Drag&drop перенос переменных: группа/модуль меняются в БД,
/// целевая группа объявляется автоматически, драйверы перезагружаются.
#[tokio::test(flavor = "multi_thread")]
async fn move_tags_between_groups() {
    let h = Harness::start().await;

    let c1 = h
        .post_json(
            "/api/connections",
            json!({
                "name": "MOVE A", "driver_type": "modbus_client", "enabled": false,
                "poll_interval_ms": 200, "config": {"ip": "127.0.0.1", "port": 15020}
            }),
        )
        .await["id"]
        .as_i64()
        .unwrap();
    let c2 = h
        .post_json(
            "/api/connections",
            json!({
                "name": "MOVE B", "driver_type": "modbus_client", "enabled": false,
                "poll_interval_ms": 200, "config": {"ip": "127.0.0.1", "port": 15021}
            }),
        )
        .await["id"]
        .as_i64()
        .unwrap();

    let t1 = h.post_json("/api/tags", json!({
        "id": 0, "connection_id": c1, "name": "mv_1", "address_str": "HR10", "data_type": "UINT16",
        "scale": 1.0, "offset_val": 0.0, "unit": "", "group_name": "Старая",
        "log_interval_ms": 1000, "deadband": 0.0
    })).await["id"].as_i64().unwrap();

    // Перенос в другую группу того же модуля
    h.post_json(
        "/api/tags/move",
        json!({ "ids": [t1], "target_connection_id": c1, "target_group": "Новая" }),
    )
    .await;
    let groups = h.get_json(&format!("/api/connections/{c1}/groups")).await;
    let names: Vec<&str> = groups["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"Новая"),
        "целевая группа объявлена: {names:?}"
    );
    let tags = h.get_json(&format!("/api/connections/{c1}/tags")).await;
    assert_eq!(tags.as_array().unwrap()[0]["group_name"], json!("Новая"));

    // Перенос в другой модуль
    h.post_json(
        "/api/tags/move",
        json!({ "ids": [t1], "target_connection_id": c2, "target_group": "Общие" }),
    )
    .await;
    let moved = h.get_json(&format!("/api/connections/{c2}/tags")).await;
    assert_eq!(
        moved.as_array().unwrap().len(),
        1,
        "тег переехал в модуль B"
    );
    assert_eq!(moved.as_array().unwrap()[0]["name"], json!("mv_1"));

    h.stop().await;
    let _ = h.tmp.path();
}

/// Вспомогательная функция для POST-экспорта PNG.
async fn post_png(h: &Harness, name: &str, body: Vec<u8>) -> reqwest::Response {
    h.http
        .post(format!("{}/api/export/png", h.base))
        .query(&[("name", name)])
        .header("content-type", "image/png")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// Экспорт PNG: сервер пишет изображение в каталог выгрузки и возвращает путь.
/// Регрессия на «тихий» экспорт: в WebView2 скачивание через <a download>
/// не доходило до пользователя, теперь файл создаёт сервер.
#[tokio::test(flavor = "multi_thread")]
async fn export_png_writes_file() {
    let h = Harness::start().await;
    let png = include_bytes!("fixtures/pixel.png").to_vec();

    // Обычный экспорт: файл на диске, путь в ответе.
    let v: Value = post_png(&h, "smoke_chart.png", png.clone())
        .await
        .json()
        .await
        .unwrap();
    let path = std::path::PathBuf::from(v["path"].as_str().unwrap_or_default());
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        "smoke_chart.png",
        "имя из запроса: {v}"
    );
    assert_eq!(
        path.parent().unwrap(),
        h.exports.as_path(),
        "файл в каталоге выгрузки: {path:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        png,
        "байты на диске равны отправленным"
    );
    assert_eq!(v["bytes"].as_i64(), Some(png.len() as i64));

    // Повторный экспорт не затирает предыдущий.
    let v2: Value = post_png(&h, "smoke_chart.png", png.clone())
        .await
        .json()
        .await
        .unwrap();
    let path2 = std::path::PathBuf::from(v2["path"].as_str().unwrap());
    assert_eq!(
        path2.file_name().unwrap().to_string_lossy(),
        "smoke_chart-1.png",
        "второй файл не перезаписал первый: {v2}"
    );
    assert_ne!(path2, path);

    // Вредоносное имя: путь и расширение вырезаются, файл остаётся в каталоге выгрузки.
    let v3: Value = post_png(&h, "../../../windows/system32/evil.exe", png.clone())
        .await
        .json()
        .await
        .unwrap();
    let path3 = std::path::PathBuf::from(v3["path"].as_str().unwrap());
    assert_eq!(
        path3.parent().unwrap(),
        h.exports.as_path(),
        "выход за каталог невозможен: {path3:?}"
    );
    assert!(
        path3.to_string_lossy().ends_with(".png"),
        "расширение всегда png: {path3:?}"
    );
    assert!(!std::path::Path::new(r"C:\windows\system32\evil.exe").exists());

    // Не-PNG отклоняется, файл не создаётся.
    let bad = post_png(&h, "bad.png", b"not a png".to_vec()).await;
    assert!(
        bad.status().is_client_error() || bad.status().is_server_error(),
        "мусор должен отклоняться"
    );
    assert_eq!(
        std::fs::read_dir(&h.exports).unwrap().count(),
        3,
        "отклонённый экспорт ничего не пишет"
    );

    // Имя без имени файла -> разумное значение по умолчанию.
    let v5: Value = post_png(&h, "", png.clone()).await.json().await.unwrap();
    let name5 = std::path::PathBuf::from(v5["path"].as_str().unwrap())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        name5.starts_with("pda_chart_") && name5.ends_with(".png"),
        "имя по умолчанию: {name5}"
    );

    h.stop().await;
    let _ = h.tmp.path();
}

/// /api/series с явными t0/t1 должен вернуть ИМЕННО это окно, а не дефолтный
/// «последний час». Регрессия на контракт зума: фронт присылает границы
/// прямо из uPlot.scales.x (секунды, float) после setScale. Данные в БД для
/// проверки не нужны — сервер обязан корректно рассчитать само окно.
#[tokio::test(flavor = "multi_thread")]
async fn series_respects_explicit_zoom_window() {
    let h = Harness::start().await;

    // Явное окно в прошлом: конец = «сейчас минус сутки», начало ещё на час раньше.
    let t1 = chrono::Local::now().timestamp_millis() as f64 / 1000.0 - 86_400.0;
    let t0 = t1 - 3_600.0;
    let v = h
        .get_json(&format!("/api/series?tags=1&t0={t0}&t1={t1}"))
        .await;
    let eps = 0.01;
    assert!(
        (v["t_start"].as_f64().unwrap() - t0).abs() < eps,
        "t_start обязан равняться переданному t0 (приоритет над span): {v}"
    );
    assert!(
        (v["t_end"].as_f64().unwrap() - t1).abs() < eps,
        "t_end обязан равняться переданному t1: {v}"
    );

    // Неотфильтрованный дефолт без t0/t1: окно = последний час (span).
    let d = h.get_json("/api/series?tags=1").await;
    let span = d["t_end"].as_f64().unwrap() - d["t_start"].as_f64().unwrap();
    assert!(
        (span - 3600.0).abs() < eps,
        "без t0/t1 живёт дефолтный span: {d}"
    );

    // t1 <= t0 — невалидное окно игнорируется, снова дефолт (не инверсия).
    let v2 = h
        .get_json(&format!("/api/series?tags=1&t0={t1}&t1={t0}"))
        .await;
    assert!(
        v2["t_end"].as_f64() > v2["t_start"].as_f64(),
        "инвертированное окно отброшено: {v2}"
    );
    let span2 = v2["t_end"].as_f64().unwrap() - v2["t_start"].as_f64().unwrap();
    assert!(
        (span2 - 3600.0).abs() < eps,
        "окно вернулось к дефолту: {v2}"
    );

    h.stop().await;
    let _ = h.tmp.path();
}

/// Экспорт PDF: страница обязана содержать график в реальную величину.
/// Регрессия на «пустой PDF»: printpdf 0.12 по умолчанию рисует изображение в
/// квадрате 1×1 и сам подставляет масштаб px→pt, только если
/// `no_auto_scale = false`. С true картинка сжималась до ~1 pt, и на странице
/// оставался белый лист. Тест проверяет, что cm-матрица перед вставкой
/// XObject-картинки (Do) имеет масштаб порядка размеров графика, а не единицы.
#[tokio::test(flavor = "multi_thread")]
async fn export_pdf_places_chart_at_real_size() {
    let h = Harness::start().await;

    // PNG «как с canvas»: 1500×700 px, непрозрачный.
    let img = image::RgbaImage::from_pixel(1500, 700, image::Rgba([20, 120, 200, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    img.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let png = png.into_inner();

    let form = reqwest::multipart::Form::new()
        .text("title", "smoke-pdf")
        .part(
            "chart_0",
            reqwest::multipart::Part::bytes(png)
                .file_name("chart_0.png")
                .mime_str("image/png")
                .unwrap(),
        )
        .text("title_0", "Тепло +1")
        .text("legend_name_0[]", "Температура")
        .text("legend_color_0[]", "rgb(255, 59, 48)");

    let r = h
        .http
        .post(format!("{}/api/export/pdf?count=1", h.base))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "pdf export -> {status}: {text}");
    let v: Value = serde_json::from_str(&text).unwrap();

    // Файл на диске — валидный PDF с одной страницей.
    let path = std::path::PathBuf::from(v["path"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), h.exports.as_path());
    let bytes = std::fs::read(&path).unwrap();
    let doc = lopdf::Document::load_mem(&bytes).expect("PDF читается");
    let pages: Vec<_> = doc.page_iter().collect();
    assert_eq!(pages.len(), 1, "одно окно = одна страница");

    // Картинка-ресурс на месте (1500×700).
    let has_img = doc.objects.values().any(|o| {
        let dict = match o {
            lopdf::Object::Dictionary(d) => d,
            lopdf::Object::Stream(s) => &s.dict,
            _ => return false,
        };
        dict.get(b"Subtype").ok().and_then(|s| s.as_name().ok()) == Some(b"Image")
            && dict.get(b"Width").ok().and_then(|w| w.as_i64().ok()) == Some(1500)
    });
    assert!(has_img, "в PDF есть Image XObject 1500 px");

    // В потоке содержимого перед Do масштаб cm обязан быть ~757×353 pt
    // (область A4 минус поля), а не 0.5 pt — признак сжатого в точку рисунка.
    let content = doc.get_and_decode_page_content(pages[0]).expect("content stream");
    let ops = &content.operations;
    let mut found = false;
    for i in 0..ops.len() {
        if ops[i].operator != "Do" {
            continue;
        }
        let Some(cm) = ops[..i].iter().rev().find(|o| o.operator == "cm") else {
            continue;
        };
        let a = cm.operands[0].as_float().unwrap_or_default();
        let d = cm.operands[3].as_float().unwrap_or_default();
        assert!(
            (700.0..800.0).contains(&a) && (320.0..400.0).contains(&d),
            "график вписан в страницу в реальную величину, а не сжат в точку: cm=[{a},{d}]"
        );
        found = true;
    }
    assert!(found, "на странице есть вставка XObject (Do)");


    h.stop().await;
    let _ = h.tmp.path();
}

