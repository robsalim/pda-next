//! pda-server как библиотека: AppCore + HTTP/WS поверх pda-core.
//!
//! Тот же код используется:
//!   src/main.rs        — автономный запуск (uvicorn-аналог из Python),
//!   tests/smoke.rs     — смоук-тест REST+WS в процессе,
//!   pda-desktop (Tauri)— встроенный сервер + открытие того же фронтенда
//!                       в нативном окне (WebView2), без отдельного exe.

pub mod api;
pub mod state;
pub mod ws;

pub use state::AppCore;

use std::path::PathBuf;

use pda_core::database::db_service::DbConfig;

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default()
}

/// Каталог фронтенда: ./web рядом с exe (продакшн), rust_pda/web (dev-запуск
/// из корня workspace), ../web от crates/pda-server (cargo run). Иначе — "web".
pub fn web_root() -> String {
    let mut candidates = vec![exe_dir().join("web"), PathBuf::from("web"), PathBuf::from("rust_pda/web")];
    if let Some(manifest) = option_env!("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest).join("../../web"));
    }
    for c in candidates {
        if c.join("index.html").is_file() {
            return c.to_string_lossy().into_owned();
        }
    }
    "web".into()
}

/// db_config.json: рядом с exe, в cwd, в rust_pda/ или в python_app/ (dev).
/// Отсутствующий файл означает SQLite по умолчанию — как в Python-версии.
/// Возвращает конфиг и путь, откуда он прочитан (для /api/db/switch).
pub fn load_db_config() -> (DbConfig, PathBuf) {
    for dir in [exe_dir(), PathBuf::from("."), PathBuf::from("rust_pda"), PathBuf::from("../python_app")] {
        let p = dir.join("db_config.json");
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                return (DbConfig::from_json(&v), p);
            }
        }
    }
    (DbConfig::default(), PathBuf::from("db_config.json"))
}
