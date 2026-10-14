//! Local HTTP/WebSocket server on 127.0.0.1.
//!
//! - `/` over WebSocket: plugins and property inspectors (Elgato SDK protocol)
//! - `/` over HTTP: the web UI
//! - `/plugins/...`: plugin files, so property inspectors load from this origin
//! - `/api/...`: JSON API for the UI

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::services::ServeDir;

use crate::api::{self, AppState};
use crate::plugins;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("ui/app.js")) }))
        .route("/ui/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("ui/style.css")) }))
        .nest_service("/plugins", ServeDir::new(plugins::plugins_dir()))
        .merge(api::routes())
        .with_state(state)
}

async fn root(State(s): State<AppState>, ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    match ws {
        Ok(ws) => {
            let host = s.host.clone();
            ws.max_message_size(64 << 20).on_upgrade(move |socket| host.handle_socket(socket))
        }
        Err(_) => Html(include_str!("ui/index.html")).into_response(),
    }
}
