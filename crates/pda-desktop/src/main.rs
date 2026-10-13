//! PDA Next — нативная оболочка (аналог Tauri: окно tao + WebView2 через wry).
//!
//! Внутри окна тот же axum-сервер (pda-server) и тот же frontend, что и в
//! браузерном режиме. Сервер стартует на 127.0.0.1 с ЭФЕМЕРАЛЬНЫМ портом,
//! поэтому desktop не конфликтует с автономно запущенным pda-server (8082).
//! Закрытие окна завершает процесс (токio-поток с сервером — демон).
//!
//! Почему tao+wry, а не пакет `tauri`: tauri-cli/сборка требуют Node.js,
//! которого в окружении нет, а на windows-gnu tauri тянет известную проблему
//! с WebView2Loader.dll (tauri#4319). tao+wry — то же окно и тот же WebView2,
//! но без npm-обвязки; frontend у нас и так статический и раздаётся сервером.

use std::sync::mpsc;

use pda_core::config::APP_NAME;
use pda_server::{api, load_db_config, AppCore};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // --- Сервер в отдельном потоке с собственным tokio-рантаймом ---
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("pda-server".into())
        .spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let (cfg, cfg_path) = load_db_config();
                let core = AppCore::from_config(cfg, cfg_path).await;
                core.manager.write().await.start_all().await;
                core.spawn_live_pump();

                // 127.0.0.1:0 -> ОС выдаёт свободный порт; фронтенд берёт host
                // из location.host, WS строится от того же адреса.
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::error!("bind: {e}");
                        return;
                    }
                };
                let addr = listener.local_addr().expect("local addr");
                let _ = tx.send(format!("http://{addr}"));
                if let Err(e) = axum::serve(listener, api::router(core)).await {
                    tracing::error!("serve: {e}");
                }
            });
        })?;

    // Ждём адрес сервера до создания окна (webview должен открыть живой URL).
    let url = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .map_err(|_| "сервер не запустился за 10 с")?;
    tracing::info!("{APP_NAME} desktop -> {url}");

    // --- Окно + WebView2 ---
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(APP_NAME)
        .with_inner_size(tao::dpi::LogicalSize::new(1360.0, 860.0))
        .build(&event_loop)?;

    let webview = WebViewBuilder::new().with_url(&url).build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        // Окно и webview живут всю жизнь цикла событий.
        let _ = (&window, &webview);
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}
