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
use crate::render::{icons, ClockColors, Flash, Renderer};
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
    Media(String, Option<crate::widgets::Media>),
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
    /// Latest MangoHud stats (None when no game is running).
    mango: Option<HashMap<String, f64>>,
    /// Now-playing info by player filter.
    media: HashMap<String, Option<crate::widgets::Media>>,
    ticks: u64,
    focus: crate::focus::FocusWatch,
    /// Page we switched to automatically, and the page to return to afterwards.
    auto_page: Option<String>,
    auto_return: Option<String>,
    regexes: HashMap<String, Option<regex::Regex>>,
    tx: mpsc::UnboundedSender<Event>,
}

/// Scans for devices forever, spawning an actor for each one that appears.
pub async fn run_manager(renderer: Arc<Renderer>, host: Arc<Host>, hub: Arc<Hub>, audio: AudioWatch, focus: crate::focus::FocusWatch) -> Result<()> {
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
                    let (renderer, host, hub, audio, focus, s) = (renderer.clone(), host.clone(), hub.clone(), audio.clone(), focus.clone(), serial.clone());
                    active.insert(
                        serial,
                        tokio::spawn(async move {
                            if let Err(e) = DeckActor::run(Arc::new(deck), kind, s.clone(), renderer, host.clone(), hub.clone(), audio, focus).await {
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
    async fn run(deck: Arc<AsyncStreamDeck>, kind: Kind, serial: String, renderer: Arc<Renderer>, host: Arc<Host>, hub: Arc<Hub>, audio: AudioWatch, mut focus: crate::focus::FocusWatch) -> Result<()> {
        focus.mark_changed();
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
            mango: None,
            media: HashMap::new(),
            ticks: 0,
            focus,
            auto_page: None,
            auto_return: None,
            regexes: HashMap::new(),
            tx,
        };
        actor.rebuild_index();
        actor.deck.reset().await?;
        actor.enter_page(None).await;
        actor.hub.register(&actor.serial, crate::hub::DeckHandle { tx: utx, kind });

        // Clocks, game stats and now-playing update once a second, only while visible.
        let mut live_tick = tokio::time::interval(Duration::from_secs(1));
        live_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = live_tick.tick() => actor.tick_live(),
                ev = rx.recv() => match ev {
                    Some(Event::Disconnected) | None => break,
                    Some(ev) => actor.on_event(ev).await,
                },
                Some(msg) = prx.recv() => actor.on_plugin(msg).await,
                Ok(()) = actor.audio.changed() => actor.mark_uuid(actions::VOLUME),
                Ok(()) = actor.focus.changed() => {
                    let f = actor.focus.borrow_and_update().clone();
                    actor.on_focus(f).await;
                },
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
            Event::Media(filter, info) => {
                if self.media.get(&filter) != Some(&info) {
                    self.media.insert(filter, info);
                    self.mark_uuid(actions::MEDIA);
                }
            }
            Event::Disconnected => {}
        }
    }

    fn tick_live(&mut self) {
        self.ticks += 1;
        if self.page_has(actions::CLOCK) {
            self.mark_uuid(actions::CLOCK);
        }
        if self.page_has(actions::MANGOHUD) {
            let stats = crate::widgets::mango_stats();
            if stats != self.mango {
                self.mango = stats;
                self.mark_uuid(actions::MANGOHUD);
            }
        }
        if self.ticks % 2 == 1 {
            for filter in self.media_filters() {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let info = crate::widgets::media(&filter).await;
                    let _ = tx.send(Event::Media(filter, info));
                });
            }
        }
    }

    /// Player filters used by now-playing actions on the visible page.
    fn media_filters(&self) -> Vec<String> {
        let Some(p) = self.cfg.pages.get(&self.current) else { return vec![] };
        let mut v: Vec<String> = p
            .keys
            .values()
            .chain(p.dials.values())
            .flat_map(|s| s.actions.iter())
            .filter(|a| a.uuid == actions::MEDIA)
            .map(|a| a.settings.get("player").and_then(Value::as_str).unwrap_or_default().to_string())
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// Input handling for the live built-ins.
    fn widget_fire(&self, a: &ActionInstance, fire: Fire) {
        match a.uuid.as_str() {
            actions::MANGOHUD if matches!(fire, Fire::Press | Fire::Touch | Fire::LongTouch) => {
                tokio::task::spawn_blocking(|| {
                    if crate::widgets::mango_toggle_hud() == 0 {
                        tracing::info!("mangohud: no running game with a control socket");
                    }
                });
            }
            actions::MEDIA => {
                let filter = a.settings.get("player").and_then(Value::as_str).unwrap_or_default();
                let Some(Some(m)) = self.media.get(filter) else { return };
                let method = match fire {
                    Fire::Rotate(t) if t > 0 => "Next",
                    Fire::Rotate(_) => "Previous",
                    _ => "PlayPause",
                };
                crate::widgets::media_call(&m.bus, method);
                let tx = self.tx.clone();
                let filter = filter.to_string();
                // Pick up the new state quickly rather than on the next poll.
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let info = crate::widgets::media(&filter).await;
                    let _ = tx.send(Event::Media(filter, info));
                });
            }
            _ => {}
        }
    }

    /// The picture for a live built-in.
    fn render_widget(&self, a: &ActionInstance, input: InputId) -> Option<DynamicImage> {
        let s = |k: &str| a.settings.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let user = a.current_state().cloned().unwrap_or_default();
        let key_size = self.kind.key_image_format().size.0 as u32;
        let (seg_w, seg_h) = match self.kind.lcd_strip_size() {
            Some((w, h)) => ((w / self.kind.encoder_count().max(1) as usize) as u32, h as u32),
            None => (0, 0),
        };
        let draw = |st: &State| match input {
            InputId::Key(_) => Some(self.renderer.render_key(key_size, Some(st), None)),
            InputId::Dial(_) => (seg_w > 0).then(|| self.renderer.render_dial(seg_w, seg_h, Some(st))),
        };
        let pos = |k: &str, default: LabelPos| match s(k).as_str() {
            "top" => LabelPos::Top,
            "center" => LabelPos::Center,
            "bottom" => LabelPos::Bottom,
            _ => default,
        };
        // A colour setting, falling back to `default` when unset.
        let color = |k: &str, default: &str| a.settings.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or(default).to_string();
        // Users can still style the widget's text through the matching label in Appearance.
        let styled = |p: LabelPos, text: String, size: f32, color: &str| {
            let u = user.labels.get(&p);
            Label {
                text,
                size: u.and_then(|l| l.size).or(Some(size)),
                color: u.and_then(|l| l.color.clone()).or(Some(color.to_string())),
            }
        };

        match a.uuid.as_str() {
            actions::CLOCK => {
                use chrono::Timelike;
                let now = chrono::Local::now();
                if s("style") == "analog" {
                    let (w, h) = if matches!(input, InputId::Key(_)) { (key_size, key_size) } else { (seg_w, seg_h) };
                    let d = ClockColors::default();
                    let colors = ClockColors {
                        face: color("face_color", &d.face),
                        rim: color("rim_color", &d.rim),
                        ticks: color("tick_color", &d.ticks),
                        hour: color("hour_color", &d.hour),
                        minute: color("minute_color", &d.minute),
                        second: color("second_color", &d.second),
                        show_seconds: a.settings.get("seconds").and_then(Value::as_bool).unwrap_or(true),
                    };
                    let time = (now.hour(), now.minute(), now.second());
                    return (w > 0).then(|| self.renderer.render_analog_clock(w, h, time, user.background.as_deref(), &colors));
                }
                let fmt = if s("format").is_empty() { "%H:%M".to_string() } else { s("format") };
                // Reject formats chrono can't handle instead of panicking mid-render.
                let mut text = String::new();
                if std::fmt::Write::write_fmt(&mut text, format_args!("{}", now.format(&fmt))).is_err() {
                    text = "bad format".into();
                }
                let mut st = State { image: user.image.clone(), background: user.background.clone(), ..Default::default() };
                st.labels.insert(LabelPos::Center, styled(LabelPos::Center, text, 20.0, &color("color", "#ffffff")));
                for (p, l) in &user.labels {
                    if *p != LabelPos::Center {
                        st.labels.insert(*p, l.clone());
                    }
                }
                draw(&st)
            }
            actions::MANGOHUD => {
                let mut st = State { image: user.image.clone(), background: user.background.clone(), ..Default::default() };
                if s("mode") == "toggle" {
                    st.labels = user.labels.clone();
                    if st.image.is_none() && st.labels.values().all(|l| l.text.is_empty()) {
                        st.labels.insert(LabelPos::Center, styled(LabelPos::Center, "HUD".into(), 22.0, &color("value_color", "#ffffff")));
                        st.labels.insert(LabelPos::Bottom, styled(LabelPos::Bottom, "MangoHud".into(), 11.0, &color("label_color", "#8a8a8a")));
                    }
                    return draw(&st);
                }
                let field = if s("field").is_empty() { "fps".to_string() } else { s("field") };
                let label = if s("label").is_empty() { field.to_uppercase().replace('_', " ") } else { s("label") };
                let value = self.mango.as_ref().and_then(|m| m.get(&field)).map(|v| crate::widgets::mango_format(&field, *v)).unwrap_or_else(|| "—".into());
                let (lp, vp) = (pos("label_pos", LabelPos::Top), pos("stat_pos", LabelPos::Center));
                st.labels.insert(lp, styled(lp, label, 13.0, &color("label_color", "#9a9a9a")));
                st.labels.insert(vp, styled(vp, value, 24.0, &color("value_color", "#ffffff")));
                draw(&st)
            }
            actions::MEDIA => {
                let info = self.media.get(&s("player")).cloned().flatten();
                let show_art = a.settings.get("art").and_then(Value::as_bool).unwrap_or(true);
                let show_title = a.settings.get("title").and_then(Value::as_bool).unwrap_or(true);
                let mut st = State { image: user.image.clone(), background: user.background.clone(), ..Default::default() };
                match info {
                    None => {
                        st.labels.insert(LabelPos::Center, styled(LabelPos::Center, "No media".into(), 12.0, &color("artist_color", "#8a8a8a")));
                    }
                    Some(m) => {
                        if show_art && st.image.is_none() {
                            st.image = m.art.clone();
                        }
                        let dial = matches!(input, InputId::Dial(_));
                        if show_title && !m.title.is_empty() {
                            let title: String = m.title.chars().take(if dial { 28 } else { 18 }).collect();
                            let p = if dial { LabelPos::Top } else { LabelPos::Bottom };
                            st.labels.insert(p, styled(p, title, if dial { 11.0 } else { 10.0 }, &color("title_color", "#ffffff")));
                        }
                        if dial && !m.artist.is_empty() {
                            st.labels.insert(LabelPos::Center, styled(LabelPos::Center, m.artist.chars().take(28).collect(), 10.0, &color("artist_color", "#b0b0b0")));
                        }
                        if !m.playing {
                            let p = if dial { LabelPos::Bottom } else { LabelPos::Top };
                            st.labels.insert(p, styled(p, "Paused".into(), 9.0, &color("paused_color", "#f7821b")));
                        }
                    }
                }
                draw(&st)
            }
            _ => None,
        }
    }

    fn page_has(&self, uuid: &str) -> bool {
        self.cfg.pages.get(&self.current).is_some_and(|p| {
            p.keys.values().chain(p.dials.values()).any(|s| s.actions.iter().any(|a| a.uuid == uuid))
        })
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
            } else if matches!(action.uuid.as_str(), actions::MANGOHUD | actions::MEDIA | actions::CLOCK) {
                self.widget_fire(action, fire);
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
            UiCmd::PluginsLoaded(uuids) => {
                let page = self.current.clone();
                for uuid in &uuids {
                    self.appear(&page, "willAppear", Some(uuid));
                }
                for k in 0..self.kind.key_count() {
                    self.dirty.insert(InputId::Key(k));
                }
                for d in 0..self.kind.encoder_count() {
                    self.dirty.insert(InputId::Dial(d));
                }
                self.redraw_dirty().await;
            }
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
        self.clear_auto();
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
        self.clear_auto();
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
                // Navigating by hand ends any automatic switch, so focus changes won't undo it.
                // Back history skips the auto page: it records the page the switcher left.
                let origin = self.auto_origin().unwrap_or_else(|| self.current.clone());
                self.clear_auto();
                self.go_to(name, Some(origin)).await;
            }
            Effect::Back => {
                // From an auto page, "back" is the page it was switched from, not history.
                if let Some(origin) = self.auto_origin() {
                    self.clear_auto();
                    self.go_to(origin, None).await;
                    return;
                }
                self.clear_auto();
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

    /// Switches page, recording `record` as the back-history entry (None leaves history alone).
    async fn go_to(&mut self, name: String, record: Option<String>) {
        if !self.cfg.pages.contains_key(&name) {
            tracing::warn!("no page named {name:?}");
            return;
        }
        if name != self.current {
            let prev = std::mem::replace(&mut self.current, name);
            if let Some(entry) = record.filter(|e| *e != self.current && self.cfg.pages.contains_key(e)) {
                self.history.push(entry);
                if self.history.len() > 32 {
                    self.history.remove(0);
                }
            }
            self.enter_page(Some(prev)).await;
        }
    }

    /// The page to treat as "where we are" for back history: the current page, or, while the
    /// auto-switcher has us on one of its pages, the page it switched away from.
    fn auto_origin(&self) -> Option<String> {
        match &self.auto_page {
            Some(page) if *page == self.current => self.auto_return.clone(),
            _ => None,
        }
    }

    fn clear_auto(&mut self) {
        self.auto_page = None;
        self.auto_return = None;
    }

    fn pattern_matches(&mut self, pattern: &str, text: &str) -> bool {
        let re = self
            .regexes
            .entry(pattern.to_string())
            .or_insert_with(|| match regex::RegexBuilder::new(pattern).case_insensitive(true).build() {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("bad auto-switch pattern {pattern:?}: {e}");
                    None
                }
            });
        re.as_ref().is_some_and(|r| r.is_match(text))
    }

    /// The first page (by name) whose focus rule matches the window.
    fn page_for_focus(&mut self, f: &crate::focus::Focus) -> Option<String> {
        let rules: Vec<(String, config::AutoSwitch)> =
            self.cfg.pages.iter().filter_map(|(n, p)| Some((n.clone(), p.auto.clone()?))).filter(|(_, a)| a.enabled).collect();
        for (name, rule) in rules {
            if rule.class.is_empty() && rule.title.is_empty() && !rule.steam_game {
                continue;
            }
            let steam_ok = !rule.steam_game || f.steam_app.is_some();
            let class_ok = rule.class.is_empty() || self.pattern_matches(&rule.class, &f.class);
            let title_ok = rule.title.is_empty() || self.pattern_matches(&rule.title, &f.title);
            if steam_ok && class_ok && title_ok {
                return Some(name);
            }
        }
        None
    }

    async fn on_focus(&mut self, f: crate::focus::Focus) {
        match self.page_for_focus(&f) {
            Some(page) if page != self.current => {
                // Remember where we came from, unless we're hopping between auto pages.
                if self.auto_page.is_none() {
                    self.auto_return = Some(self.current.clone());
                }
                match f.steam_app {
                    Some(app) => tracing::info!("[{}] focus {:?} (steam app {app}) -> page {page}", self.serial, f.class),
                    None => tracing::info!("[{}] focus {:?} -> page {page}", self.serial, f.class),
                }
                self.auto_page = Some(page.clone());
                self.go_to(page, None).await;
            }
            Some(_) => {}
            None => {
                let Some(page) = self.auto_page.take() else { return };
                let back = self.auto_return.take();
                let stay = self.cfg.pages.get(&page).and_then(|p| p.auto.as_ref()).is_some_and(|a| a.stay);
                if !stay && self.current == page {
                    if let Some(back) = back.filter(|b| self.cfg.pages.contains_key(b)) {
                        tracing::info!("[{}] focus left {page}; back to {back}", self.serial);
                        self.go_to(back, None).await;
                    }
                }
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
        self.ticks = 0;
        self.tick_live();
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
        let info_arc = self.host.action(&a.uuid);
        let info = info_arc.as_deref();
        let mut st = info.and_then(|i| i.states.get(a.state).or(i.states.first())).cloned().unwrap_or_default();
        if let Some(user) = a.current_state() {
            if user.image.is_some() {
                st.image = user.image.clone();
            }
            if user.background.is_some() {
                st.background = user.background.clone();
            }
            apply_user_labels(&mut st.labels, &user.labels);
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
                        if let Some(user) = a.current_state() {
                            apply_user_labels(&mut st.labels, &user.labels);
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
            Fire::Press | Fire::Touch | Fire::LongTouch => {
                let key = if matches!(fire, Fire::Press) { "press" } else { "touch" };
                let fallback = if key == "press" && !device.is_empty() { "default" } else { "mute" };
                match a.settings.get(key).and_then(Value::as_str).unwrap_or(fallback) {
                    "mute" => audio::toggle_mute(&device, &kind),
                    "default" => audio::set_default(&device, &kind),
                    _ => {}
                }
            }
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
    fn volume_view(&self, a: &ActionInstance) -> (String, String, Option<u32>, bool, bool) {
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
        // A dial following "Default output" is always the default, so marking it says nothing.
        let default = if kind == "source" { &audio.default_source } else { &audio.default_sink };
        let is_default = !device.is_empty() && device == *default;
        (icon, name, vol.map(|v| v.percent), vol.is_some_and(|v| v.muted), is_default)
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
        if let Some(a) = action.filter(|a| matches!(a.uuid.as_str(), actions::CLOCK | actions::MANGOHUD | actions::MEDIA)) {
            return self.render_widget(a, input);
        }
        if let Some(a) = action.filter(|a| a.uuid == actions::VOLUME) {
            let (icon, name, percent, muted, is_default) = self.volume_view(a);
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
                    Some(self.renderer.render_volume(seg, h as u32, &icon, &name, percent, muted, is_default, flash))
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
                        let plugin_dir = self.host.plugin(&info.plugin).map(|p| p.dir.clone());
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
                        let title = st.and_then(|s| s.labels.values().find(|l| !l.text.is_empty())).map(|l| l.text.clone()).unwrap_or_else(|| info.name.clone());
                        layout.set_default("title", Value::String(title));
                        // A label colour chosen in Appearance wins over the plugin's.
                        if let Some(c) = a.current_state().and_then(|u| u.labels.values().find_map(|l| l.color.clone())) {
                            layout.set_prop("title", "color", Value::String(c));
                        }
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

/// Puts the user's labels over `labels`. Labels without text only restyle whatever
/// ends up at their position, such as the manifest title or a plugin's live title.
fn apply_user_labels(labels: &mut std::collections::BTreeMap<LabelPos, Label>, user: &std::collections::BTreeMap<LabelPos, Label>) {
    if user.values().any(|l| !l.text.is_empty()) {
        *labels = user.clone();
        return;
    }
    for (pos, u) in user {
        let l = labels.entry(*pos).or_default();
        if u.size.is_some() {
            l.size = u.size;
        }
        if u.color.is_some() {
            l.color = u.color.clone();
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
