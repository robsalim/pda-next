//! PDA Next — автономный HTTP+WebSocket сервер (аналог uvicorn из Python).
//!
//! Ядро (AppCore) и маршруты (api::router) живут в библиотеке pda-server:
//! их же использует pda-desktop, поэтому main здесь тонкий — конфиг БД,
//! запуск драйверов, live-рассылка и bind на WEB_HOST:WEB_PORT.
//!
//! Порт можно переопределить без правки кода: аргумент --port N или PDA_PORT
//! (PDA_HOST для адреса). Это нужно для dev-запуска рядом с desktop-оболочкой.

use pda_core::config::{APP_NAME, APP_VERSION, WEB_HOST, WEB_PORT};
use pda_server::{api, load_db_config, AppCore};

/// Читаем override из argv/env: "pda-server --port 9000" / PDA_PORT=9000.
fn bind_addr() -> (String, u16) {
    let mut args = std::env::args().skip(1);
    let mut host = std::env::var("PDA_HOST").unwrap_or_else(|_| WEB_HOST.to_string());
    let mut port = std::env::var("PDA_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(WEB_PORT);

    while let Some(a) = args.next() {
        match a.as_str() {
            "--host" => {
                if let Some(v) = args.next() {
                    host = v;
                }
            }
            "--port" => {
                if let Some(v) = args.next() {
                    match v.parse::<u16>() {
                        Ok(p) => port = p,
                        Err(_) => eprintln!("Некорректный --port: {v} (ожидалось число 1..65535), беру {port}"),
                    }
                }
            }
            "-h" | "--help" => {
                println!("{} {} — HTTP+WebSocket сервер\n", APP_NAME, APP_VERSION);
                println!("Использование: pda-server [OPTIONS]");
                println!("  --host ADDR   адрес для bind (по умолчанию {WEB_HOST})");
                println!("  --port PORT   порт для bind (по умолчанию {WEB_PORT})");
                std::process::exit(0);
            }
            other => tracing::warn!("Неизвестный аргумент: {other}"),
        }
    }
    (host, port)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let (host, port) = bind_addr();

    // --- Ядро: база из db_config.json, драйверы, live-рассылка ---
    let (cfg, cfg_path) = load_db_config();
    tracing::info!("{APP_NAME} {APP_VERSION}: база {}", cfg_path.display());
    let core = AppCore::from_config(cfg, cfg_path).await;
    core.manager.write().await.start_all().await;
    core.spawn_live_pump();

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    let addr = listener.local_addr()?;
    tracing::info!("{APP_NAME} сервер слушает http://{addr} (фронтенд из {:?})", pda_server::web_root());

    axum::serve(listener, api::router(core)).await?;
    Ok(())
}
