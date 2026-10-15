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
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::{Theme, WindowBuilder};
use wry::WebViewBuilder;

/// Событие из webview в событийный цикл окна. Тема страницы живёт в JS
/// (localStorage), а цвет заголовка окна — нативный, поэтому синхронизация
/// идёт через IPC: app.js шлёт "theme:dark" / "theme:light".
#[derive(Debug, Clone, Copy)]
enum UiEvent {
    /// true — тёмная тема.
    Theme(bool),
}

fn theme_to_ui(dark: bool) -> Theme {
    if dark { Theme::Dark } else { Theme::Light }
}

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
    // EventLoop типизирован UiEvent: через прокси из IPC-обработчика приходит
    // смена темы страницы (см. UiEvent). EventLoop::new() умеет только (),
    // пользовательские события требуют builder.
    let event_loop = EventLoopBuilder::<UiEvent>::with_user_event().build();
    // Тёмная шапка по умолчанию: тема страницы тоже тёмная, если в браузере
    // ещё не переключали. Дальше тему синхронизирует app.js при загрузке.
    let window = WindowBuilder::new()
        .with_title(APP_NAME)
        .with_inner_size(tao::dpi::LogicalSize::new(1360.0, 860.0))
        .with_theme(Some(Theme::Dark))
        .build(&event_loop)?;

    let proxy = event_loop.create_proxy();
    let webview = WebViewBuilder::new()
        .with_url(&url)
        // Тема страницы переключается в JS, а заголовок окна рисует Windows.
        // tao сам прокидывает DWMWA_USE_IMMERSIVE_DARK_MODE по set_theme.
        .with_ipc_handler(move |req| {
            let dark = !req.body().contains("light");
            let _ = proxy.send_event(UiEvent::Theme(dark));
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        // Окно и webview живут всю жизнь цикла событий.
        let _ = (&window, &webview);
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(UiEvent::Theme(dark)) => window.set_theme(Some(theme_to_ui(dark))),
            _ => {}
        }
    });
}
