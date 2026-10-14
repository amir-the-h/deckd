mod actions;
mod audio;
mod config;
mod device;
mod importer;
mod installer;
mod layout;
mod plugins;
mod render;
mod server;
mod usbreset;

use std::sync::Arc;

use anyhow::Result;

const USAGE: &str = "\
deckd - lightweight Stream Deck daemon

USAGE:
    deckd                          run the daemon
    deckd devices                  list connected Stream Decks
    deckd install PATH...          install .streamDeckPlugin files or .sdPlugin folders
    deckd import-streamcontroller [DIR] [--columns N]
                                   import pages from StreamController
    deckd preview SERIAL PAGE OUT.png
                                   render a Stream Deck+ page to an image";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "deckd=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {
            let renderer = Arc::new(render::Renderer::new()?);
            let port: u16 = std::env::var("DECKD_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(28196);
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            let host = plugins::Host::load(port);
            tracing::info!("listening on http://127.0.0.1:{port}");
            let app = server::router(host.clone());
            tokio::select! {
                r = device::run_manager(renderer, host, audio::start()) => r?,
                r = async { axum::serve(listener, app).await } => r?,
                _ = shutdown_signal() => tracing::info!("shutting down"),
            }
        }
        Some("install") if args.len() >= 2 => {
            for path in &args[1..] {
                if let Err(e) = installer::install(std::path::Path::new(path)) {
                    eprintln!("{path}: {e:#}");
                }
            }
        }
        Some("devices") => {
            let hid = elgato_streamdeck::new_hidapi()?;
            for (kind, serial) in elgato_streamdeck::list_devices(&hid) {
                println!("{serial}\t{kind:?}");
            }
        }
        Some("import-streamcontroller") => {
            let mut dir = importer::default_sc_dir();
            let mut columns = 4;
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--columns" => columns = it.next().and_then(|v| v.parse().ok()).unwrap_or(4),
                    path => dir = path.into(),
                }
            }
            println!("importing from {}", dir.display());
            importer::import(&dir, columns)?;
        }
        Some("preview") if args.len() >= 4 => {
            preview(&args[1], &args[2], &args[3])?;
        }
        _ => println!("{USAGE}"),
    }
    Ok(())
}

/// Renders a page as it would appear on a Stream Deck+ (4x2 keys over an 800x100 strip).
fn preview(serial: &str, page: &str, out: &str) -> Result<()> {
    use image::{imageops, Rgba, RgbaImage};
    let cfg = config::load_device(serial)?;
    let page = cfg.pages.get(page).ok_or_else(|| anyhow::anyhow!("no page {page:?}"))?;
    let r = render::Renderer::new()?;
    let mut canvas = RgbaImage::from_pixel(820, 400, Rgba([40, 40, 40, 255]));
    for k in 0..8u8 {
        let state = page.keys.get(&k).and_then(config::Slot::display_action).and_then(|a| a.current_state());
        let img = r.render_key(120, state, None);
        let (x, y) = ((k % 4) as i64 * 200 + 50, (k / 4) as i64 * 140 + 10);
        imageops::overlay(&mut canvas, &img, x, y);
    }
    for d in 0..4u8 {
        let state = page.dials.get(&d).and_then(config::Slot::display_action).and_then(|a| a.current_state());
        let img = r.render_dial(200, 100, state);
        imageops::overlay(&mut canvas, &img, d as i64 * 200 + 10, 290);
    }
    canvas.save(out)?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
