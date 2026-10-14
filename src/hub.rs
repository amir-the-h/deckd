//! Connects the web API to device actors, and fans out change events to UI clients.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use elgato_streamdeck::info::Kind;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::{Page, Profile};
use crate::device::InputId;

pub type Reply<T> = oneshot::Sender<Result<T, String>>;

/// Requests from the web UI to a device actor, which owns the deck's config.
pub enum UiCmd {
    Snapshot(oneshot::Sender<Snapshot>),
    PutPage { name: String, page: Page, reply: Reply<Page> },
    DeletePage { name: String, reply: Reply<()> },
    RenamePage { from: String, to: String, reply: Reply<()> },
    SetProfile { profile: Profile, reply: Reply<()> },
    Switch { page: String },
    Render { page: String, input: InputId, reply: oneshot::Sender<Option<Vec<u8>>> },
    /// Opens (or closes) a property inspector; returns Elgato's `inActionInfo`.
    Inspector { context: String, open: bool, reply: Reply<Value> },
}

#[derive(Clone, serde::Serialize)]
pub struct Snapshot {
    pub profile: Profile,
    pub current: String,
    pub pages: BTreeMap<String, Page>,
}

#[derive(Clone)]
pub struct DeckHandle {
    pub tx: mpsc::UnboundedSender<UiCmd>,
    pub kind: Kind,
}

pub struct Hub {
    decks: Mutex<HashMap<String, DeckHandle>>,
    events: broadcast::Sender<String>,
}

impl Hub {
    pub fn new() -> Self {
        Self { decks: Mutex::new(HashMap::new()), events: broadcast::channel(256).0 }
    }

    pub fn register(&self, serial: &str, handle: DeckHandle) {
        self.decks.lock().unwrap().insert(serial.to_string(), handle);
        self.emit(serde_json::json!({ "type": "devices" }));
    }

    pub fn unregister(&self, serial: &str) {
        self.decks.lock().unwrap().remove(serial);
        self.emit(serde_json::json!({ "type": "devices" }));
    }

    pub fn deck(&self, serial: &str) -> Option<DeckHandle> {
        self.decks.lock().unwrap().get(serial).cloned()
    }

    pub fn decks(&self) -> Vec<(String, DeckHandle)> {
        let mut v: Vec<_> = self.decks.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Broadcasts an event to connected UI clients. Free when nobody listens.
    pub fn emit(&self, event: Value) {
        if self.events.receiver_count() > 0 {
            let _ = self.events.send(event.to_string());
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }
}
