//! Local HTTP/WebSocket server on 127.0.0.1.
//!
//! - `/` over WebSocket: plugins and property inspectors (Elgato SDK protocol)
//! - `/` over HTTP: the web UI
//! - `/plugins/...`: plugin files, so property inspectors load from this origin
//! - `/api/...`: JSON API for the UI
//!
//! Every request goes through [`guard`]: the API can run commands and install
//! plugins, so websites open in a browser must not be able to reach it.

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Request, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::services::ServeDir;

use crate::api::{self, AppState};
use crate::plugins;

pub fn router(state: AppState, port: u16) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/ui/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], include_str!("ui/app.js")) }))
        .route("/ui/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css")], include_str!("ui/style.css")) }))
        .nest_service("/plugins", ServeDir::new(plugins::plugins_dir()))
        .merge(api::routes())
        .with_state(state)
        .layer(middleware::from_fn(move |req: Request, next: Next| guard(port, req, next)))
}

/// Rejects requests that didn't come from deckd's own UI, a plugin or a local tool.
///
/// - `Host` must be a loopback name, which defeats DNS rebinding (a website whose
///   domain is re-pointed at 127.0.0.1 still sends its own name as the Host).
/// - `Origin`, when present, must be this server or `file://` (HTML plugins run
///   from disk). Browsers attach it to cross-site requests and WebSocket
///   handshakes; plugin processes and curl don't send one.
async fn guard(port: u16, req: Request, next: Next) -> Response {
    if allowed(req.headers(), port) {
        next.run(req).await
    } else {
        tracing::warn!("refused {} {} from host {:?} origin {:?}", req.method(), req.uri(), req.headers().get(header::HOST), req.headers().get(header::ORIGIN));
        StatusCode::FORBIDDEN.into_response()
    }
}

fn allowed(headers: &HeaderMap, port: u16) -> bool {
    let local = |authority: &str| {
        let host = authority.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()).map_or(authority, |(h, _)| h);
        matches!(host, "127.0.0.1" | "localhost" | "[::1]")
    };
    let host_ok = headers.get(header::HOST).and_then(|h| h.to_str().ok()).is_some_and(local);
    let origin_ok = match headers.get(header::ORIGIN).map(|o| o.to_str()) {
        None => true,
        Some(Ok(o)) => {
            o == "file://"
                || o.strip_prefix("http://").is_some_and(|a| local(a) && a.rsplit_once(':').is_some_and(|(_, p)| p == port.to_string()))
        }
        Some(Err(_)) => false,
    };
    host_ok && origin_ok
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
