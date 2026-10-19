# Stream Deck SDK protocol as implemented by deckd

deckd follows Elgato's SDK v2 WebSocket protocol. This file lists exactly what deckd sends and handles, so you don't build on something it doesn't do. Source of truth: `src/device.rs` (per-device events) and `src/plugins/mod.rs` (connection, routing, global settings).

## Contents
- Connection and registration
- Events deckd sends to plugins
- Messages plugins can send
- Property inspector (settings panel)
- Not implemented

## Connection and registration

Plugin is launched with `-port <port> -pluginUUID <uuid> -registerEvent registerPlugin -info <json>`, then:

```js
const ws = new WebSocket(`ws://127.0.0.1:${port}`);
ws.onopen = () => ws.send(JSON.stringify({ event: "registerPlugin", uuid: pluginUUID }));
```

Messages for your plugin that arrive before you register are queued and delivered right after registration, so `willAppear` for on-screen actions comes immediately after `deviceDidConnect`.

`-info` JSON:
```json
{
  "application": { "language": "en", "platform": "linux", "version": "6.9.0", "font": "", "platformVersion": "" },
  "plugin": { "uuid": "com.example.myplugin", "version": "1.0.0" },
  "devicePixelRatio": 2,
  "colors": { "highlightColor": "#007AFFFF", "...": "..." },
  "devices": [{ "id": "A00XX0000ABCDE", "name": "Stream Deck Plus", "size": { "columns": 4, "rows": 2 }, "type": 7 }]
}
```
Device `type` uses Elgato's ids: 0 Stream Deck, 1 Mini, 2 XL, 5 Pedal, 7 Stream Deck+, 9 Neo.

## Events deckd sends to plugins

Common envelope for action events:
```json
{
  "event": "keyDown",
  "action": "com.example.myplugin.hello",
  "context": "1f3c...",                 // stable per action instance, survives restarts
  "device": "A00XX0000ABCDE",
  "payload": {
    "settings": {},                      // always an object
    "coordinates": { "column": 2, "row": 1 },   // dials: { "column": <dial index>, "row": 0 }
    "controller": "Keypad",              // or "Encoder"
    "state": 0,
    "isInMultiAction": false
  }
}
```

| Event | When | Extra payload |
|---|---|---|
| `deviceDidConnect` / `deviceDidDisconnect` | on registration and hot-plug | `deviceInfo` (connect only) |
| `willAppear` / `willDisappear` | action enters/leaves the visible page; also after a plugin restart and when the page is edited in the UI | — |
| `keyDown` / `keyUp` | key press/release. Also sent as a pair for actions bound to a tap/hold/turn trigger, and for key-only actions placed on a dial | — |
| `dialDown` / `dialUp` | dial pushed/released | — |
| `dialPress` | legacy (pre-6.1) twin of dialDown/dialUp; ignore it if you handle those | `pressed: bool` |
| `dialRotate` | dial turned | `ticks` (signed), `pressed` (held while turning) |
| `touchTap` | touch strip tapped over this dial's segment | `tapPos: [x, y]` (segment-local), `hold: bool` (long press) |
| `didReceiveSettings` | reply to `getSettings`; after the settings panel saves; after a UI edit changes settings | `settings` |
| `didReceiveGlobalSettings` | reply to `getGlobalSettings`; after the settings panel saves globals | `settings` |
| `sendToPlugin` | your settings panel sent data | arbitrary `payload` |
| `propertyInspectorDidAppear` / `...DidDisappear` | settings panel opened/closed in the UI | — |

## Messages plugins can send

| Event | Effect in deckd |
|---|---|
| `setTitle` `{title, state?, target?}` | Overrides the title (not persisted). `null` restores the configured title; `""` hides it. `\n` makes multiple lines. Without `state`, applies to all states. `target` is ignored. |
| `setImage` `{image, state?}` | Data URI (`data:image/png;base64,...`, `data:image/svg+xml;base64,...`, `data:image/svg+xml,<svg...>`). Empty/null resets. On a dial it sets the layout's `icon`; an image at least 1.5× wider than tall is drawn full-segment. |
| `setState` `{state}` | Switches to that manifest state (persisted). |
| `setFeedback` `{key: value \| {value, color, opacity, enabled, ...}}` | Updates touch-strip layout items. Bars take numbers (`range` default 0–100). |
| `setFeedbackLayout` `{layout}` | `$X1`, `$A0`, `$A1`, `$B1`, `$B2`, `$C1`, a layout JSON path relative to the plugin, or an inline layout object. Clears previous feedback. |
| `setSettings` `{...}` | Persists to the page file; forwarded to the open settings panel as `didReceiveSettings`. |
| `getSettings` | Replies `didReceiveSettings`. |
| `setGlobalSettings` / `getGlobalSettings` | Stored in `~/.config/deckd/plugin-data/<uuid>.json`; use your plugin UUID as `context`. |
| `showOk` / `showAlert` | Green "OK" / red "!" overlay for 1.2 s. |
| `sendToPropertyInspector` `{payload}` | Delivered to the open settings panel for that context. |
| `openUrl` `{url}` | `xdg-open`. |
| `logMessage` `{message}` | Written to deckd's journal as `[<plugin>] message`. |
| `switchToProfile` `{profile}` | Switches to the deckd page with that name. |
| `setTriggerDescription` | Accepted and ignored. |

### Built-in layouts (200×100 canvas)
- `$X1` — `title` (top, centred) + `icon` (48×48, centre)
- `$A0` — `title` + `full-canvas` (200×100 pixmap)
- `$A1` — `title` + `icon` (left) + `value` (text, right)
- `$B1` — `$A1` + `indicator` (bar)
- `$B2` — `$A1` + `indicator` (gradient bar, `bar_fill_c` like `"0:#ff0000,1:#00ff00"`)
- `$C1` — `icon1`/`indicator1` + `icon2`/`indicator2`

Bars that never receive a value aren't drawn.

## Property inspector (settings panel)

The UI loads `PropertyInspectorPath` from `http://127.0.0.1:<port>/plugins/<folder>/<path>` in an iframe and calls the page's global:

```js
connectElgatoStreamDeckSocket(port, uuid /* = context */, "registerPropertyInspector", infoJson, actionInfoJson)
```

`actionInfoJson` is `{"action", "context", "device", "payload": {"settings", "coordinates", "controller", "state"}}`. Register with `{"event": "registerPropertyInspector", "uuid": uuid}`. The panel can send `setSettings`, `getSettings`, `sendToPlugin`, `setGlobalSettings`, `getGlobalSettings`, `openUrl`, `logMessage`. The iframe grows to fit its content. Elgato's `sdpi` CSS works if you bundle it; the UI background behind the panel is `#2d2d2d`.

## Not implemented

deckd does not send `applicationDidLaunch`/`applicationDidTerminate` (ApplicationsToMonitor), `systemDidWakeUp`, `titleParametersDidChange`, or `didReceiveDeepLink`. Multi-actions aren't a separate concept — users stack several actions in one slot, each with its own trigger, and every action gets its own events.
