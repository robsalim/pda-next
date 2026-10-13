//! WebSocket /api/live/ws — толкает live-снимки реестра подписчикам.
//!
//! Python-фронтенд жил на поллинге /api/live; WS даёт тот же payload чаще
//! без HTTP-шторма. Формат: {"type":"live","rows":[{id,name,value,quality,timestamp},...]}.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::StreamExt;
use tokio::sync::broadcast::error::RecvError;

use crate::state::AppCore;

pub async fn live_ws(State(core): State<Arc<AppCore>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| pump(socket, core))
}

async fn pump(mut socket: WebSocket, core: Arc<AppCore>) {
    // Первая доставка сразу при подключении, затем — из broadcast-канала.
    let mut rx = core.live_tx.subscribe();
    if let Ok(first) = rx.recv().await {
        if socket.send(Message::Text(first)).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(text) => {
                    if socket.send(Message::Text(text)).await.is_err() {
                        return; // клиент закрыл соединение
                    }
                }
                // Отстали от рассылки — не догоняем прошлые кадры, берём свежий.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
            // Читаем сокет, чтобы видеть close/ping от клиента.
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Close(_))) | None => return,
                Some(Ok(Message::Ping(data))) => {
                    if socket.send(Message::Pong(data)).await.is_err() { return; }
                }
                Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}
