//! One actor per connected Stream Deck: reads input, dispatches actions, draws.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use elgato_streamdeck::asynchronous::{list_devices_async, refresh_device_list_async, AsyncStreamDeck};
use elgato_streamdeck::images::ImageRect;
use elgato_streamdeck::info::Kind;
use elgato_streamdeck::{new_hidapi, DeviceStateUpdate};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::actions::{self, Effect, Fire};
use crate::config::{self, ActionInstance, DeviceConfig, Label, LabelPos, Slot, State, Trigger};
use crate::layout::Layout;
use crate::plugins::{Host, ToDeck};
use crate::audio::{self, AudioWatch};
use crate::hub::{Hub, UiCmd};
use crate::render::{icons, Flash, Renderer};
use image::DynamicImage;

const HOLD_THRESHOLD: Duration = Duration::from_millis(500);
const FLASH_TIME: Duration = Duration::from_millis(1200);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputId {
    Key(u8),
    Dial(u8),
}

#[derive(Debug)]
enum Event {
    Hw(DeviceStateUpdate),
    HoldTimer(InputId, u64),
    Redraw(String),
    Disconnected,
}

struct Press {
    id: u64,
    page: String,
    held: bool,
}

/// Where an action instance lives, for looking it up by context.
#[derive(Debug, Clone)]
struct Loc {
    page: String,
    input: InputId,
    index: usize,
}

/// State plugins push at runtime. Not persisted, like in Elgato's software.
#[derive(Debug, Default)]
struct Live {
    titles: HashMap<usize, Option<String>>,
    images: HashMap<usize, String>,
    feedback: Map<String, Value>,
    layout: Option<Value>,
    flash: Option<(Flash, Instant)>,
}

pub struct DeckActor {
    deck: Arc<AsyncStreamDeck>,
    kind: Kind,
    serial: String,
    renderer: Arc<Renderer>,
    host: Arc<Host>,
    hub: Arc<Hub>,
    audio: AudioWatch,
    cfg: DeviceConfig,
    index: HashMap<String, Loc>,
    current: String,
    history: Vec<String>,
    brightness: u8,
    presses: HashMap<InputId, Press>,
    next_press: u64,
    live: HashMap<String, Live>,
    dirty: HashSet<InputId>,
    tx: mpsc::UnboundedSender<Event>,
}

/// Scans for devices forever, spawning an actor for each one that appears.
pub async fn run_manager(renderer: Arc<Renderer>, host: Arc<Host>, hub: Arc<Hub>, audio: AudioWatch) -> Result<()> {
    let mut hid = new_hidapi()?;
    let mut active: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut recovery = crate::usbreset::Recovery::default();
    loop {
        active.retain(|_, h| !h.is_finished());
        recovery.check();
        if let Err(e) = refresh_device_list_async(&mut hid) {
            tracing::warn!("hid refresh: {e}");
        }
        for (kind, serial) in list_devices_async(&hid) {
            if active.contains_key(&serial) || !kind.is_visual() {
                continue;
            }
            match AsyncStreamDeck::connect(&hid, kind, &serial) {
                Ok(deck) => {
                    tracing::info!("connected {kind:?} {serial}");
                    let (renderer, host, hub, audio, s) = (renderer.clone(), host.clone(), hub.clone(), audio.clone(), serial.clone());
                    active.insert(
                        serial,
                        tokio::spawn(async move {
                            if let Err(e) = DeckActor::run(Arc::new(deck), kind, s.clone(), renderer, host.clone(), hub.clone(), audio).await {
                                tracing::error!("deck {s}: {e:#}");
                            }
                            host.unregister_deck(&s);
                            hub.unregister(&s);
                            tracing::info!("deck {s} gone");
                        }),
                    );
                }
                Err(e) => tracing::warn!("connect {serial}: {e} (is another app holding the device?)"),
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

impl DeckActor {
    async fn run(deck: Arc<AsyncStreamDeck>, kind: Kind, serial: String, renderer: Arc<Renderer>, host: Arc<Host>, hub: Arc<Hub>, audio: AudioWatch) -> Result<()> {
        let cfg = config::load_device(&serial)?;
        let current = if cfg.pages.contains_key(&cfg.profile.default_page) {
            cfg.profile.default_page.clone()
        } else {
            cfg.pages.keys().next().cloned().unwrap_or_default()
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (ptx, mut prx) = mpsc::unbounded_channel();
        host.register_deck(&serial, kind, ptx);
        let (utx, mut urx) = mpsc::unbounded_channel();

        let reader = deck.get_reader();
        let rtx = tx.clone();
        tokio::spawn(async move {
            loop {
                match reader.read(100.0).await {
                    Ok(updates) => {
                        for u in updates {
                            if rtx.send(Event::Hw(u)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("read: {e}");
                        let _ = rtx.send(Event::Disconnected);
                        return;
                    }
                }
            }
        });

        let mut actor = DeckActor {
            deck,
            kind,
            serial,
            renderer,
            host,
            hub,
            audio,
            brightness: cfg.profile.brightness,
            index: HashMap::new(),
            cfg,
            current,
            history: Vec::new(),
            presses: HashMap::new(),
            next_press: 0,
            live: HashMap::new(),
            dirty: HashSet::new(),
            tx,
        };
        actor.rebuild_index();
        actor.deck.reset().await?;
        actor.enter_page(None).await;
        actor.hub.register(&actor.serial, crate::hub::DeckHandle { tx: utx, kind });

        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(Event::Disconnected) | None => break,
                    Some(ev) => actor.on_event(ev).await,
                },
                Some(msg) = prx.recv() => actor.on_plugin(msg).await,
                Ok(()) = actor.audio.changed() => actor.mark_uuid(actions::VOLUME),
                Some(cmd) = urx.recv() => actor.on_ui(cmd).await,
            }
            // Coalesce bursts (e.g. a plugin animating feedback) into one redraw.
            while let Ok(msg) = prx.try_recv() {
                actor.on_plugin(msg).await;
            }
            actor.redraw_dirty().await;
        }
        Ok(())
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (name, page) in &self.cfg.pages {
            let slots = page.keys.iter().map(|(k, s)| (InputId::Key(*k), s)).chain(page.dials.iter().map(|(d, s)| (InputId::Dial(*d), s)));
            for (input, slot) in slots {
                for (index, a) in slot.actions.iter().enumerate() {
                    self.index.insert(a.context.clone(), Loc { page: name.clone(), input, index });
                }
            }
        }
    }

    fn slot(&self, page: &str, input: InputId) -> Option<&Slot> {
        let page = self.cfg.pages.get(page)?;
        match input {
            InputId::Key(k) => page.keys.get(&k),
            InputId::Dial(d) => page.dials.get(&d),
        }
    }

    fn instance_mut(&mut self, ctx: &str) -> Option<(&Loc, &mut ActionInstance)> {
        let loc = self.index.get(ctx)?;
        let page = self.cfg.pages.get_mut(&loc.page)?;
        let slot = match loc.input {
            InputId::Key(k) => page.keys.get_mut(&k),
            InputId::Dial(d) => page.dials.get_mut(&d),
        }?;
        Some((loc, slot.actions.get_mut(loc.index)?))
    }

    fn is_plugin(&self, a: &ActionInstance) -> bool {
        self.host.action(&a.uuid).is_some()
    }

    async fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Hw(u) => self.on_hw(u).await,
            Event::HoldTimer(input, id) => self.on_hold(input, id).await,
            Event::Redraw(ctx) => {
                if let Some(live) = self.live.get_mut(&ctx) {
                    if live.flash.is_some_and(|(_, t)| t.elapsed() >= FLASH_TIME) {
                        live.flash = None;
                    }
                }
                self.mark_ctx(&ctx);
            }
            Event::Disconnected => {}
        }
    }

    // ---------------------------------------------------------------- input

    async fn on_hw(&mut self, u: DeviceStateUpdate) {
        tracing::debug!("{u:?}");
        let page = self.current.clone();
        match u {
            DeviceStateUpdate::ButtonDown(k) => self.press_down(InputId::Key(k)).await,
            DeviceStateUpdate::ButtonUp(k) => self.press_up(InputId::Key(k)).await,
            DeviceStateUpdate::EncoderDown(d) => self.press_down(InputId::Dial(d)).await,
            DeviceStateUpdate::EncoderUp(d) => self.press_up(InputId::Dial(d)).await,
            DeviceStateUpdate::EncoderTwist(d, ticks) => {
                let input = InputId::Dial(d);
                let pressed = self.presses.contains_key(&input);
                let dir = if ticks > 0 { Trigger::Cw } else { Trigger::Ccw };
                self.dispatch(input, &page, Fire::Rotate(ticks), |t| t.is_none(), |me, a| {
                    if me.supports_encoder(a) {
                        me.send_plugin("dialRotate", a, input, json!({ "ticks": ticks, "pressed": pressed }));
                    }
                })
                .await;
                // Direction-bound actions act like a button click per turn.
                self.dispatch(input, &page, Fire::Press, |t| t == Some(dir), |me, a| me.send_click(a, input)).await;
            }
            DeviceStateUpdate::TouchScreenPress(x, y) | DeviceStateUpdate::TouchScreenLongPress(x, y) => {
                let long = matches!(u, DeviceStateUpdate::TouchScreenLongPress(..));
                let (d, local_x) = self.segment_at(x);
                let input = InputId::Dial(d);
                let fire = if long { Fire::LongTouch } else { Fire::Touch };
                self.dispatch(input, &page, fire, |_| true, |me, a| {
                    if me.supports_encoder(a) {
                        me.send_plugin("touchTap", a, input, json!({ "tapPos": [local_x, y], "hold": long }));
                    } else {
                        me.send_click(a, input);
                    }
                })
                .await;
            }
            DeviceStateUpdate::TouchScreenSwipe((x0, _), (x1, _)) => {
                tracing::debug!("swipe {x0} -> {x1}");
            }
            DeviceStateUpdate::TouchPointDown(_) | DeviceStateUpdate::TouchPointUp(_) => {}
        }
    }

    fn segment_at(&self, x: u16) -> (u8, u16) {
        let w = self.kind.lcd_strip_size().map(|(w, _)| w).unwrap_or(800) as u16;
        let n = self.kind.encoder_count().max(1) as u16;
        let seg = (x * n / w).min(n - 1);
        (seg as u8, x - seg * (w / n))
    }

    async fn press_down(&mut self, input: InputId) {
        self.next_press += 1;
        let id = self.next_press;
        let page = self.current.clone();
        self.presses.insert(input, Press { id, page: page.clone(), held: false });

        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(HOLD_THRESHOLD).await;
            let _ = tx.send(Event::HoldTimer(input, id));
        });

        // Untriggered plugin actions get the raw down event; triggered ones get a full click later.
        self.dispatch(input, &page, Fire::Press, |t| t.is_none(), |me, a| me.send_down(a, input)).await;
        self.dispatch(input, &page, Fire::Press, |t| t == Some(Trigger::Down), |me, a| me.send_click(a, input)).await;
    }

    async fn on_hold(&mut self, input: InputId, id: u64) {
        let Some(press) = self.presses.get_mut(&input).filter(|p| p.id == id) else { return };
        press.held = true;
        let page = press.page.clone();
        self.dispatch(input, &page, Fire::Press, |t| t == Some(Trigger::Hold), |me, a| me.send_click(a, input)).await;
    }

    async fn press_up(&mut self, input: InputId) {
        // Release goes to the page the press started on, even if the press switched pages.
        let Some(press) = self.presses.remove(&input) else { return };
        let page = press.page;
        for a in self.plugin_actions(&page, input, |t| t.is_none()) {
            self.send_up(&a, input);
        }
        if !press.held {
            self.dispatch(input, &page, Fire::Press, |t| t == Some(Trigger::Tap), |me, a| me.send_click(a, input)).await;
        }
    }

    fn plugin_actions(&self, page: &str, input: InputId, filter: impl Fn(Option<Trigger>) -> bool) -> Vec<ActionInstance> {
        self.slot(page, input)
            .map(|s| s.actions.iter().filter(|a| filter(a.trigger) && self.is_plugin(a)).cloned().collect())
            .unwrap_or_default()
    }

    /// Runs built-ins and hands plugin actions to `on_plugin` for every action whose trigger passes `filter`.
    async fn dispatch(
        &mut self,
        input: InputId,
        page: &str,
        fire: Fire,
        filter: impl Fn(Option<Trigger>) -> bool,
        on_plugin: impl Fn(&Self, &ActionInstance),
    ) {
        let Some(slot) = self.slot(page, input) else { return };
        let mut effects = Vec::new();
        for action in &slot.actions {
            // Presses and turns honour triggers; touch reaches every untriggered action.
            let pass = match fire {
                Fire::Press | Fire::Rotate(_) => filter(action.trigger),
                Fire::Touch | Fire::LongTouch => action.trigger.is_none(),
            };
            if !pass {
                continue;
            }
            if action.uuid == actions::VOLUME {
                self.volume_fire(action, fire);
            } else if actions::is_builtin(&action.uuid) {
                if let Some(e) = actions::run(&action.uuid, &action.settings, fire) {
                    effects.push(e);
                }
            } else if self.is_plugin(action) {
                on_plugin(self, action);
            } else {
                tracing::debug!("no handler for {} ({fire:?})", action.uuid);
            }
        }
        for e in effects {
            self.apply(e).await;
        }
    }

    // --------------------------------------------------------- plugin events

    fn coordinates(&self, input: InputId) -> Value {
        match input {
            InputId::Key(k) => {
                let cols = self.kind.column_count().max(1);
                json!({ "column": k % cols, "row": k / cols })
            }
            InputId::Dial(d) => json!({ "column": d, "row": 0 }),
        }
    }

    fn send_plugin(&self, event: &str, a: &ActionInstance, input: InputId, extra: Value) {
        let mut payload = json!({
            "settings": if a.settings.is_object() { a.settings.clone() } else { json!({}) },
            "coordinates": self.coordinates(input),
            "controller": if matches!(input, InputId::Dial(_)) { "Encoder" } else { "Keypad" },
            "state": a.state,
            "isInMultiAction": false,
        });
        if let (Some(p), Value::Object(extra)) = (payload.as_object_mut(), extra) {
            p.extend(extra);
        }
        self.host.send_to_action(
            &a.uuid,
            json!({ "action": a.uuid, "event": event, "context": a.context, "device": self.serial, "payload": payload }),
        );
    }

    /// Whether the action declares dial support; key-only actions on a dial get key events instead.
    fn supports_encoder(&self, a: &ActionInstance) -> bool {
        self.host.action(&a.uuid).is_some_and(|i| i.encoder)
    }

    fn send_down(&self, a: &ActionInstance, input: InputId) {
        match input {
            InputId::Dial(_) if !self.supports_encoder(a) => self.send_plugin("keyDown", a, input, json!({ "controller": "Keypad" })),
            InputId::Key(_) => self.send_plugin("keyDown", a, input, json!({})),
            InputId::Dial(_) => {
                self.send_plugin("dialDown", a, input, json!({}));
                // Pre-6.1 plugins only understand dialPress.
                self.send_plugin("dialPress", a, input, json!({ "pressed": true }));
            }
        }
    }

    fn send_up(&self, a: &ActionInstance, input: InputId) {
        match input {
            InputId::Dial(_) if !self.supports_encoder(a) => self.send_plugin("keyUp", a, input, json!({ "controller": "Keypad" })),
            InputId::Key(_) => self.send_plugin("keyUp", a, input, json!({})),
            InputId::Dial(_) => {
                self.send_plugin("dialUp", a, input, json!({}));
                self.send_plugin("dialPress", a, input, json!({ "pressed": false }));
            }
        }
    }

    fn send_click(&self, a: &ActionInstance, input: InputId) {
        if self.is_plugin(a) {
            self.send_down(a, input);
            self.send_up(a, input);
        }
    }

    fn appear(&self, page: &str, event: &str, only_plugin: Option<&str>) {
        let Some(p) = self.cfg.pages.get(page) else { return };
        let slots = p.keys.iter().map(|(k, s)| (InputId::Key(*k), s)).chain(p.dials.iter().map(|(d, s)| (InputId::Dial(*d), s)));
        for (input, slot) in slots {
            for a in &slot.actions {
                let Some(info) = self.host.action(&a.uuid) else { continue };
                if only_plugin.is_some_and(|p| p != info.plugin) {
                    continue;
                }
                if event == "willAppear" {
                    self.host.register_context(&a.context, &self.serial, &a.uuid);
                }
                self.send_plugin(event, a, input, json!({}));
            }
        }
    }

    async fn on_plugin(&mut self, msg: ToDeck) {
        let (v, from_inspector) = match msg {
            ToDeck::FromPlugin(v) => (v, false),
            ToDeck::FromInspector(v) => (v, true),
        };
        let event = v.get("event").and_then(Value::as_str).unwrap_or_default().to_string();
        let ctx = v.get("context").and_then(Value::as_str).unwrap_or_default().to_string();
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);

        match event.as_str() {
            "deckd:reappear" => {
                let plugin = v.get("plugin").and_then(Value::as_str).map(String::from);
                self.appear(&self.current.clone(), "willAppear", plugin.as_deref());
            }
            "setTitle" | "setImage" => {
                let n_states = self.state_count(&ctx);
                let live = self.live.entry(ctx.clone()).or_default();
                let target_states: Vec<usize> = match payload.get("state").and_then(Value::as_u64) {
                    Some(s) => vec![s as usize],
                    None => (0..n_states).collect(),
                };
                for s in target_states {
                    if event == "setTitle" {
                        let title = payload.get("title").and_then(Value::as_str).map(String::from);
                        live.titles.insert(s, title);
                    } else {
                        match payload.get("image").and_then(Value::as_str).filter(|i| !i.is_empty()) {
                            Some(img) => live.images.insert(s, img.to_string()),
                            None => live.images.remove(&s),
                        };
                    }
                }
                self.mark_ctx(&ctx);
            }
            "setState" => {
                if let Some(s) = payload.get("state").and_then(Value::as_u64) {
                    if let Some((_, a)) = self.instance_mut(&ctx) {
                        a.state = s as usize;
                    }
                    self.mark_ctx(&ctx);
                }
            }
            "setFeedback" => {
                if let Value::Object(fb) = payload {
                    self.live.entry(ctx.clone()).or_default().feedback.extend(fb);
                    self.mark_ctx(&ctx);
                }
            }
            "setFeedbackLayout" => {
                if let Some(layout) = payload.get("layout") {
                    let live = self.live.entry(ctx.clone()).or_default();
                    live.layout = Some(layout.clone());
                    live.feedback.clear();
                    self.mark_ctx(&ctx);
                }
            }
            "showAlert" | "showOk" => {
                let flash = if event == "showOk" { Flash::Ok } else { Flash::Alert };
                self.live.entry(ctx.clone()).or_default().flash = Some((flash, Instant::now()));
                self.mark_ctx(&ctx);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(FLASH_TIME).await;
                    let _ = tx.send(Event::Redraw(ctx));
                });
            }
            "setSettings" => {
                let serial = self.serial.clone();
                let Some((loc, a)) = self.instance_mut(&ctx) else { return };
                a.settings = payload.clone();
                let (uuid, page, input) = (a.uuid.clone(), loc.page.clone(), loc.input);
                self.save_page(&page);
                self.hub.emit(json!({ "type": "settings", "serial": self.serial, "context": ctx }));
                let msg = json!({
                    "action": uuid, "event": "didReceiveSettings", "context": ctx, "device": serial,
                    "payload": { "settings": payload, "coordinates": self.coordinates(input), "isInMultiAction": false },
                });
                // Elgato forwards settings to the other side only.
                if from_inspector {
                    self.host.send_to_action(&uuid, msg);
                } else {
                    self.host.send_to_inspector(&ctx, msg);
                }
            }
            "getSettings" => {
                let Some(loc) = self.index.get(&ctx).cloned() else { return };
                let Some(a) = self.slot(&loc.page, loc.input).and_then(|s| s.actions.get(loc.index)) else { return };
                let msg = json!({
                    "action": a.uuid, "event": "didReceiveSettings", "context": ctx, "device": self.serial,
                    "payload": {
                        "settings": if a.settings.is_object() { a.settings.clone() } else { json!({}) },
                        "coordinates": self.coordinates(loc.input), "isInMultiAction": false,
                    },
                });
                if from_inspector {
                    self.host.send_to_inspector(&ctx, msg);
                } else {
                    self.host.send_to_action(&a.uuid.clone(), msg);
                }
            }
            "switchToProfile" => {
                if let Some(p) = payload.get("profile").and_then(Value::as_str) {
                    self.apply(Effect::SwitchPage(p.to_string())).await;
                }
            }
            "setTriggerDescription" => {}
            other => tracing::debug!("unhandled plugin event {other}"),
        }
    }

    fn state_count(&self, ctx: &str) -> usize {
        let Some(loc) = self.index.get(ctx) else { return 1 };
        let Some(a) = self.slot(&loc.page, loc.input).and_then(|s| s.actions.get(loc.index)) else { return 1 };
        let manifest = self.host.action(&a.uuid).map(|i| i.states.len()).unwrap_or(0);
        manifest.max(a.states.len()).max(1)
    }

    fn save_page(&self, name: &str) {
        if let Some(page) = self.cfg.pages.get(name) {
            if let Err(e) = config::write_json(&self.cfg.dir.join("pages").join(format!("{name}.json")), page) {
                tracing::warn!("saving page {name}: {e:#}");
            }
        }
    }


    // ------------------------------------------------------------- web UI

    async fn on_ui(&mut self, cmd: UiCmd) {
        match cmd {
            UiCmd::Snapshot(reply) => {
                let _ = reply.send(crate::hub::Snapshot {
                    profile: self.cfg.profile.clone(),
                    current: self.current.clone(),
                    pages: self.cfg.pages.clone(),
                });
            }
            UiCmd::PutPage { name, page, reply } => {
                let _ = reply.send(self.put_page(name, page).await);
            }
            UiCmd::DeletePage { name, reply } => {
                let _ = reply.send(self.delete_page(&name).await);
            }
            UiCmd::RenamePage { from, to, reply } => {
                let _ = reply.send(self.rename_page(&from, &to));
            }
            UiCmd::SetProfile { profile, reply } => {
                self.cfg.profile = profile;
                self.brightness = self.cfg.profile.brightness;
                let page_brightness = self.cfg.pages.get(&self.current).and_then(|p| p.brightness);
                let _ = self.deck.set_brightness(page_brightness.unwrap_or(self.brightness)).await;
                let r = config::write_json(&self.cfg.dir.join("profile.json"), &self.cfg.profile).map_err(|e| format!("{e:#}"));
                self.hub.emit(json!({ "type": "config", "serial": self.serial }));
                let _ = reply.send(r);
            }
            UiCmd::Switch { page } => self.apply(Effect::SwitchPage(page)).await,
            UiCmd::Render { page, input, reply } => {
                let png = self.render(&page, input).and_then(|img| {
                    let mut buf = std::io::Cursor::new(Vec::new());
                    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
                    Some(buf.into_inner())
                });
                let _ = reply.send(png);
            }
            UiCmd::Inspector { context, open, reply } => {
                let Some(loc) = self.index.get(&context).cloned() else {
                    let _ = reply.send(Err("unknown action".into()));
                    return;
                };
                let Some(a) = self.slot(&loc.page, loc.input).and_then(|s| s.actions.get(loc.index)).cloned() else {
                    let _ = reply.send(Err("unknown action".into()));
                    return;
                };
                self.host.register_context(&context, &self.serial, &a.uuid);
                let event = if open { "propertyInspectorDidAppear" } else { "propertyInspectorDidDisappear" };
                self.host.send_to_action(&a.uuid, json!({ "action": a.uuid, "event": event, "context": context, "device": self.serial }));
                let controller = if matches!(loc.input, InputId::Dial(_)) && self.supports_encoder(&a) { "Encoder" } else { "Keypad" };
                let _ = reply.send(Ok(json!({
                    "action": a.uuid,
                    "context": context,
                    "device": self.serial,
                    "payload": {
                        "settings": if a.settings.is_object() { a.settings.clone() } else { json!({}) },
                        "coordinates": self.coordinates(loc.input),
                        "controller": controller,
                        "state": a.state,
                        "isInMultiAction": false,
                    },
                })));
            }
        }
    }

    /// Every action instance on a page, by context.
    fn instances(page: &config::Page) -> HashMap<String, (InputId, ActionInstance)> {
        page.keys
            .iter()
            .map(|(k, s)| (InputId::Key(*k), s))
            .chain(page.dials.iter().map(|(d, s)| (InputId::Dial(*d), s)))
            .flat_map(|(input, slot)| slot.actions.iter().map(move |a| (a.context.clone(), (input, a.clone()))))
            .collect()
    }

    async fn put_page(&mut self, name: String, mut page: config::Page) -> Result<config::Page, String> {
        if !config::valid_page_name(&name) {
            return Err(format!("invalid page name {name:?}"));
        }
        config::assign_contexts(&mut page);
        let visible = name == self.current;
        let old = self.cfg.pages.get(&name).map(Self::instances).unwrap_or_default();
        let new = Self::instances(&page);

        // Tell plugins about what changed on the visible page, like Elgato's app does.
        if visible {
            for (ctx, (input, a)) in &old {
                let moved = new.get(ctx).is_none_or(|(i, n)| *i != *input || n.uuid != a.uuid);
                if moved && self.is_plugin(a) {
                    self.send_plugin("willDisappear", a, *input, json!({}));
                }
            }
        }
        for ctx in old.keys().filter(|c| !new.contains_key(*c)) {
            self.live.remove(ctx);
        }
        self.cfg.pages.insert(name.clone(), page.clone());
        self.save_page(&name);
        self.rebuild_index();
        if visible {
            for (ctx, (input, a)) in &new {
                if !self.is_plugin(a) {
                    continue;
                }
                match old.get(ctx) {
                    Some((i, o)) if *i == *input && o.uuid == a.uuid => {
                        if o.settings != a.settings {
                            self.send_plugin("didReceiveSettings", a, *input, json!({}));
                        }
                    }
                    _ => {
                        self.host.register_context(ctx, &self.serial, &a.uuid);
                        self.send_plugin("willAppear", a, *input, json!({}));
                    }
                }
            }
            let page_brightness = page.brightness;
            let _ = self.deck.set_brightness(page_brightness.unwrap_or(self.brightness)).await;
            for k in 0..self.kind.key_count() {
                self.dirty.insert(InputId::Key(k));
            }
            for d in 0..self.kind.encoder_count() {
                self.dirty.insert(InputId::Dial(d));
            }
        }
        self.hub.emit(json!({ "type": "config", "serial": self.serial, "page": name }));
        Ok(page)
    }

    async fn delete_page(&mut self, name: &str) -> Result<(), String> {
        if !self.cfg.pages.contains_key(name) {
            return Err(format!("no page {name:?}"));
        }
        if self.cfg.pages.len() == 1 {
            return Err("can't delete the only page".into());
        }
        self.cfg.pages.remove(name);
        let _ = std::fs::remove_file(self.cfg.dir.join("pages").join(format!("{name}.json")));
        self.history.retain(|p| p != name);
        if self.cfg.profile.default_page == name {
            self.cfg.profile.default_page = self.cfg.pages.keys().next().cloned().unwrap_or_default();
            let _ = config::write_json(&self.cfg.dir.join("profile.json"), &self.cfg.profile);
        }
        self.rebuild_index();
        if self.current == name {
            self.current = self.cfg.profile.default_page.clone();
            self.enter_page(None).await;
        }
        self.hub.emit(json!({ "type": "config", "serial": self.serial }));
        Ok(())
    }

    fn rename_page(&mut self, from: &str, to: &str) -> Result<(), String> {
        if !config::valid_page_name(to) {
            return Err(format!("invalid page name {to:?}"));
        }
        if self.cfg.pages.contains_key(to) {
            return Err(format!("a page named {to:?} already exists"));
        }
        let page = self.cfg.pages.remove(from).ok_or_else(|| format!("no page {from:?}"))?;
        self.cfg.pages.insert(to.to_string(), page);
        let pages_dir = self.cfg.dir.join("pages");
        let _ = std::fs::rename(pages_dir.join(format!("{from}.json")), pages_dir.join(format!("{to}.json")));

        // Keep page links pointing at the renamed page.
        let mut touched = Vec::new();
        for (name, page) in self.cfg.pages.iter_mut() {
            for slot in page.keys.values_mut().chain(page.dials.values_mut()) {
                for a in &mut slot.actions {
                    if a.uuid == actions::PAGE && a.settings.get("page").and_then(Value::as_str) == Some(from) {
                        a.settings["page"] = json!(to);
                        touched.push(name.clone());
                    }
                }
            }
        }
        touched.dedup();
        for name in touched {
            self.save_page(&name);
        }
        if self.cfg.profile.default_page == from {
            self.cfg.profile.default_page = to.to_string();
            let _ = config::write_json(&self.cfg.dir.join("profile.json"), &self.cfg.profile);
        }
        if self.current == from {
            self.current = to.to_string();
        }
        for p in &mut self.history {
            if p == from {
                *p = to.to_string();
            }
        }
        self.rebuild_index();
        self.hub.emit(json!({ "type": "config", "serial": self.serial }));
        Ok(())
    }

    // ------------------------------------------------------------ navigation

    async fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::SwitchPage(name) => {
                if !self.cfg.pages.contains_key(&name) {
                    tracing::warn!("no page named {name:?}");
                    return;
                }
                if name != self.current {
                    let prev = std::mem::replace(&mut self.current, name);
                    self.history.push(prev.clone());
                    if self.history.len() > 32 {
                        self.history.remove(0);
                    }
                    self.enter_page(Some(prev)).await;
                }
            }
            Effect::Back => {
                if let Some(prev) = self.history.pop() {
                    let old = std::mem::replace(&mut self.current, prev);
                    self.enter_page(Some(old)).await;
                }
            }
            Effect::Brightness { delta } => {
                self.brightness = (self.brightness as i32 + delta).clamp(0, 100) as u8;
                let _ = self.deck.set_brightness(self.brightness).await;
            }
        }
    }

    async fn enter_page(&mut self, previous: Option<String>) {
        tracing::info!("[{}] page {}", self.serial, self.current);
        self.hub.emit(json!({ "type": "page", "serial": self.serial, "page": self.current }));
        if let Some(prev) = previous {
            self.appear(&prev, "willDisappear", None);
        }
        self.appear(&self.current.clone(), "willAppear", None);
        let page_brightness = self.cfg.pages.get(&self.current).and_then(|p| p.brightness);
        let _ = self.deck.set_brightness(page_brightness.unwrap_or(self.brightness)).await;
        for k in 0..self.kind.key_count() {
            self.dirty.insert(InputId::Key(k));
        }
        for d in 0..self.kind.encoder_count() {
            self.dirty.insert(InputId::Dial(d));
        }
        self.redraw_dirty().await;
    }

    // --------------------------------------------------------------- drawing

    fn mark_ctx(&mut self, ctx: &str) {
        if let Some(loc) = self.index.get(ctx) {
            if loc.page == self.current {
                self.dirty.insert(loc.input);
            }
        }
    }

    async fn redraw_dirty(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let dirty: Vec<InputId> = self.dirty.drain().collect();
        for input in dirty {
            self.draw(input).await;
        }
        if let Err(e) = self.deck.flush().await {
            tracing::warn!("flush: {e}");
        }
    }

    /// Merges manifest defaults, the user's saved appearance and live plugin updates.
    fn resolve_state(&self, a: &ActionInstance) -> State {
        let info = self.host.action(&a.uuid);
        let mut st = info.and_then(|i| i.states.get(a.state).or(i.states.first())).cloned().unwrap_or_default();
        if let Some(user) = a.current_state() {
            if user.image.is_some() {
                st.image = user.image.clone();
            }
            if user.background.is_some() {
                st.background = user.background.clone();
            }
            if !user.labels.is_empty() {
                st.labels = user.labels.clone();
            }
        }
        if let Some(live) = self.live.get(&a.context) {
            if let Some(img) = live.images.get(&a.state) {
                st.image = Some(img.clone());
            }
            if let Some(title) = live.titles.get(&a.state) {
                let pos = st.labels.keys().next().copied().unwrap_or(LabelPos::Bottom);
                let style = st.labels.remove(&pos).unwrap_or_default();
                st.labels.clear();
                match title {
                    Some(t) if !t.is_empty() => {
                        st.labels.insert(pos, Label { text: t.clone(), ..style });
                    }
                    Some(_) => {}
                    // A null title restores the configured one.
                    None => {
                        st.labels = info.and_then(|i| i.states.get(a.state)).map(|s| s.labels.clone()).unwrap_or_default();
                        if let Some(user) = a.current_state().filter(|u| !u.labels.is_empty()) {
                            st.labels = user.labels.clone();
                        }
                    }
                }
            }
        }
        st
    }

    fn volume_fire(&self, a: &ActionInstance, fire: Fire) {
        let (device, kind, step) = volume_settings(a);
        match fire {
            Fire::Rotate(t) => {
                let current = self.audio.borrow().get(&device).map(|v| v.percent).unwrap_or(0);
                audio::adjust(&device, &kind, current, t as i32 * step);
            }
            Fire::Press | Fire::Touch | Fire::LongTouch => audio::toggle_mute(&device, &kind),
        }
    }

    /// Marks every visible slot whose displayed action has `uuid` for redraw.
    fn mark_uuid(&mut self, uuid: &str) {
        let Some(page) = self.cfg.pages.get(&self.current) else { return };
        let inputs: Vec<InputId> = page
            .keys
            .iter()
            .map(|(k, s)| (InputId::Key(*k), s))
            .chain(page.dials.iter().map(|(d, s)| (InputId::Dial(*d), s)))
            .filter(|(_, s)| s.display_action().is_some_and(|a| a.uuid == uuid))
            .map(|(i, _)| i)
            .collect();
        self.dirty.extend(inputs);
    }

    /// Picture for the native volume action: (icon, name, percent, muted).
    fn volume_view(&self, a: &ActionInstance) -> (String, String, Option<u32>, bool) {
        let (device, kind, _) = volume_settings(a);
        let audio = self.audio.borrow();
        let vol = audio.get(&device);
        let s = |k: &str| a.settings.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
        let icon = match s("icon") {
            Some("headset") => icons::HEADSET.to_string(),
            Some("mic") => icons::MIC.to_string(),
            Some("speaker") => icons::SPEAKER.to_string(),
            Some(path) => path.to_string(),
            None if kind == "source" => icons::MIC.to_string(),
            None => icons::SPEAKER.to_string(),
        };
        let name = s("name").map(String::from).or_else(|| vol.map(|v| v.description.clone())).unwrap_or_else(|| "Volume".into());
        (icon, name, vol.map(|v| v.percent), vol.is_some_and(|v| v.muted))
    }

    async fn draw(&self, input: InputId) {
        let Some(img) = self.render(&self.current, input) else { return };
        match input {
            InputId::Key(k) => {
                if let Err(e) = self.deck.set_button_image(k, img).await {
                    tracing::warn!("key {k}: {e}");
                }
            }
            InputId::Dial(d) => {
                let seg = img.width() as u16;
                match ImageRect::from_image_async(img) {
                    Ok(rect) => {
                        if let Err(e) = self.deck.write_lcd(d as u16 * seg, 0, &rect).await {
                            tracing::warn!("lcd {d}: {e}");
                        }
                    }
                    Err(e) => tracing::warn!("lcd encode: {e}"),
                }
            }
        }
        self.hub.emit(json!({ "type": "render", "serial": self.serial, "input": input_name(input) }));
    }

    /// Renders one slot of any page as it would appear on the device.
    fn render(&self, page: &str, input: InputId) -> Option<DynamicImage> {
        let action = self.slot(page, input).and_then(Slot::display_action);
        if let Some(a) = action.filter(|a| a.uuid == actions::VOLUME) {
            let (icon, name, percent, muted) = self.volume_view(a);
            let flash = self.live.get(&a.context).and_then(|l| l.flash.map(|(f, _)| f));
            return match input {
                InputId::Key(_) => {
                    let size = self.kind.key_image_format().size.0 as u32;
                    let mut st = State { image: Some(icon), ..Default::default() };
                    let text = match percent {
                        Some(_) if muted => "Muted".to_string(),
                        Some(p) => format!("{p}%"),
                        None => name,
                    };
                    st.labels.insert(LabelPos::Bottom, Label { text, ..Default::default() });
                    Some(self.renderer.render_key(size, Some(&st), flash))
                }
                InputId::Dial(_) => {
                    let (w, h) = self.kind.lcd_strip_size()?;
                    let seg = (w / self.kind.encoder_count() as usize) as u32;
                    Some(self.renderer.render_volume(seg, h as u32, &icon, &name, percent, muted, flash))
                }
            };
        }
        let state = action.map(|a| self.resolve_state(a));
        let flash = action.and_then(|a| self.live.get(&a.context)).and_then(|l| l.flash.map(|(f, _)| f));
        match input {
            InputId::Key(_) => {
                let size = self.kind.key_image_format().size.0 as u32;
                Some(self.renderer.render_key(size, state.as_ref(), flash))
            }
            InputId::Dial(_) => {
                let (w, h) = self.kind.lcd_strip_size()?;
                let seg = (w / self.kind.encoder_count() as usize) as u32;
                let img = match action.and_then(|a| Some((a, self.host.action(&a.uuid)?))) {
                    // Key-only actions on a dial: show their key picture big, with the title beside it.
                    Some((_, info)) if !info.encoder => self.renderer.render_dial(seg, h as u32, state.as_ref()),
                    Some((a, info)) => {
                        let live = self.live.get(&a.context);
                        let plugin_dir = self.host.plugins.get(&info.plugin).map(|p| p.dir.clone());
                        let live_image = live.and_then(|l| l.images.get(&a.state));
                        // Plugins written for OpenDeck draw the whole segment themselves and send it
                        // via setImage. Show such wide images full-size instead of as the layout icon.
                        let full_canvas = live_image.filter(|img| {
                            live.is_some_and(|l| l.layout.is_none() && l.feedback.is_empty())
                                && self.renderer.image_dims(img).is_some_and(|(iw, ih)| iw as f32 >= ih as f32 * 1.5)
                        });
                        let mut layout = if let Some(img) = full_canvas {
                            let mut l = Layout::resolve(&Value::String("$A0".into()), None);
                            l.items.retain(|i| i.key == "full-canvas");
                            l.set_default("full-canvas", Value::String(img.clone()));
                            l
                        } else {
                            let spec = live.and_then(|l| l.layout.clone()).unwrap_or_else(|| Value::String(info.encoder_layout.clone()));
                            Layout::resolve(&spec, plugin_dir.as_deref())
                        };
                        if let Some(l) = live {
                            layout.apply(&l.feedback);
                        }
                        let st = state.as_ref();
                        let title = st.and_then(|s| s.labels.values().next()).map(|l| l.text.clone()).unwrap_or_else(|| info.name.clone());
                        layout.set_default("title", Value::String(title));
                        let user_image = a.current_state().and_then(|s| s.image.clone());
                        let icon = if live_image.is_some() || user_image.is_some() {
                            st.and_then(|s| s.image.clone())
                        } else {
                            info.encoder_icon.clone().or_else(|| st.and_then(|s| s.image.clone()))
                        };
                        if let Some(icon) = icon {
                            layout.set_default("icon", Value::String(icon));
                        }
                        // Bars the plugin never set would otherwise draw as empty tracks.
                        layout.items.retain(|i| !matches!(i.kind, crate::layout::ItemKind::Bar | crate::layout::ItemKind::GBar) || live.is_some_and(|l| l.feedback.contains_key(&i.key)));
                        self.renderer.render_layout(seg, h as u32, &layout, info.encoder_background.as_deref(), plugin_dir.as_deref(), flash)
                    }
                    None => self.renderer.render_dial(seg, h as u32, state.as_ref()),
                };
                Some(img)
            }
        }
    }
}

/// (device, "sink" | "source", step percent) for a `deckd.volume` action.
fn volume_settings(a: &ActionInstance) -> (String, String, i32) {
    let s = |k: &str| a.settings.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let kind = if s("kind") == "source" { "source".to_string() } else { "sink".to_string() };
    let step = a.settings.get("step").and_then(Value::as_i64).unwrap_or(2) as i32;
    (s("device"), kind, step)
}

pub fn input_name(input: InputId) -> String {
    match input {
        InputId::Key(k) => format!("key-{k}"),
        InputId::Dial(d) => format!("dial-{d}"),
    }
}

pub fn parse_input(s: &str) -> Option<InputId> {
    let (kind, n) = s.split_once('-')?;
    let n: u8 = n.parse().ok()?;
    match kind {
        "key" => Some(InputId::Key(n)),
        "dial" => Some(InputId::Dial(n)),
        _ => None,
    }
}
