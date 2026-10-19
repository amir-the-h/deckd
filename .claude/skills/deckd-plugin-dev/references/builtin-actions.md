# Adding a built-in action to deckd (Rust)

Built-ins run inside the daemon: no process, no WebSocket, direct access to device state. Existing ones are good templates — pick the closest:

| Built-in | Pattern | Where to look |
|---|---|---|
| `deckd.command`, `deckd.page`, `deckd.dial` | stateless: input → run something / navigate | `actions::run` in `src/actions.rs` |
| `deckd.volume` | live external state pushed by a watcher, custom drawing | `src/audio.rs`, `volume_fire`/`volume_view` in `src/device.rs` |
| `deckd.clock`, `deckd.media`, `deckd.mangohud` | redrawn on a 1 s tick while visible | `src/widgets.rs`, `tick_live`/`render_widget` |
| `deckd.mediakey` | input → async call, default icon from settings | `media_key_fire` in `src/device.rs`, `widgets::media_key` |

## Steps

### 1. Name it — `src/actions.rs`
```rust
pub const MYTHING: &str = "deckd.mything";
```
UUIDs starting with `deckd.` are treated as built-ins (`actions::is_builtin`).

### 2. Behaviour

**Stateless** (fire-and-forget): add an arm to `actions::run(uuid, settings, fire)`. `Fire` is `Press`, `Rotate(ticks)`, `Touch` or `LongTouch`. Return `Some(Effect::…)` to ask the device to do something (switch page, brightness), else `None`. Use `spawn_shell` for commands — it detaches and reaps the child.

**Needs device state or mutation:** handle it in `DeckActor::dispatch` (`src/device.rs`) next to the `VOLUME` branch. `dispatch` only has `&self`; to change settings or state, return a new `Effect` variant (declare it in `actions.rs`) and handle it in `DeckActor::apply`, which has `&mut self`. To persist a settings change there, update the instance's `settings` (`instance_mut`), call `save_page`, `mark_ctx` (redraw) and emit a `settings` hub event so the web UI refreshes.

Triggers are applied before your code: for presses, `dispatch` only calls you for actions whose trigger passes the filter, so you don't handle tap/hold yourself.

### 3. Drawing — `DeckActor::render`
Add a branch near the top, before the generic plugin rendering:
```rust
if let Some(a) = action.filter(|a| a.uuid == actions::MYTHING) {
    let mut st = State::default();
    st.labels.insert(LabelPos::Center, Label { text: "hi".into(), size: Some(20.0), color: None });
    if let Some(user) = a.current_state() { st.image = user.image.clone(); st.background = user.background.clone(); }
    return match input {
        InputId::Key(_) => Some(self.renderer.render_key(self.kind.key_image_format().size.0 as u32, Some(&st), None)),
        InputId::Dial(_) => { let (w, h) = self.kind.lcd_strip_size()?; Some(self.renderer.render_dial((w / self.kind.encoder_count() as usize) as u32, h as u32, Some(&st))) }
    };
}
```
Renderer helpers (`src/render.rs`): `render_key`, `render_dial` (icon left, labels right), `render_layout` (Elgato feedback layouts), `render_volume`, `render_analog_clock`. Labels support `\n`. `State.image` accepts file paths, data URIs and `glyph:<set>:<icon>?color=…&scale=…` icon-library references. Respect the user's chosen icon/background from `a.current_state()` so the Appearance panel still works.

**Live updates:** call `self.mark_uuid(actions::MYTHING)` (redraws every visible slot using it) or `self.mark_ctx(&context)`. For periodic refresh, add it to `tick_live` (1 s, only while `page_has(...)`). For async data, spawn a task and send an `Event` variant back through `self.tx`, as `Event::Media` does. Never block the actor: use `tokio::process`, or `spawn_blocking` for sync I/O.

### 4. Show it in the UI
- **Catalog:** add an entry to `builtins()` in `src/api.rs` — `uuid`, `name`, `tooltip`, `keypad`, `encoder`, `icon` (use the `icon("<svg path d>")` helper with a 24×24 Material path).
- **Settings form:** add a branch in `renderSettings` in `src/ui/app.js` using `settingInput(a, key, attrs)`, `settingSelect(a, key, options, onChange)`, `settingCheckbox(a, key, label, default)`. They save automatically. Call `refreshSlot(S.sel)` after changes that alter the picture.

### 5. Import from StreamController (optional)
If it replaces a StreamController action, map it in `convert_action` in `src/importer.rs`: `"plugin_id::Action" => (actions::MYTHING, json!({...}), vec![])`.

### 6. Build and deploy
```bash
cargo build --release
cp target/release/deckd ~/.local/bin/deckd.new && mv ~/.local/bin/deckd.new ~/.local/bin/deckd
systemctl --user restart deckd
```
Check the result without touching the hardware with the render endpoint:
`curl -o /tmp/k.png http://127.0.0.1:28196/api/devices/<serial>/render/<page>/key-0` (or `dial-0`). Commit when it works.
