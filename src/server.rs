//! Local HTTP/WebSocket server. Plugins and property inspectors connect to `/`
//! over WebSocket, as the Elgato SDK expects; the web UI will live alongside.

use std::sync::Arc;

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::plugins::Host;

pub fn router(host: Arc<Host>) -> Router {
    Router::new().route("/", get(root)).with_state(host)
}

async fn root(State(host): State<Arc<Host>>, ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    match ws {
        Ok(ws) => ws.max_message_size(64 << 20).on_upgrade(move |socket| host.handle_socket(socket)),
        Err(_) => "deckd is running".into_response(),
    }
}
