---
name: deckd-plugin-dev
description: How to build, install, test and debug plugins and actions for deckd, the Stream Deck daemon in this repo — both Elgato-format plugins (Node.js, HTML or native .sdPlugin folders that deckd hosts over the Stream Deck SDK WebSocket protocol) and native built-in actions written in Rust inside deckd. Use this whenever the user wants a new Stream Deck button, dial or touch-strip behaviour, a custom action, an integration with some app or service, a property inspector (settings panel), help porting a StreamController or Elgato plugin, or is debugging why a plugin doesn't start, register, draw or receive events in deckd — even if they just say "make a button that…" or "add an action for…".
---

# Developing plugins for deckd

deckd runs Stream Deck devices without the Elgato app. It hosts **Elgato-format plugins** (the same `.sdPlugin` folders the official software uses) and also has **built-in actions** compiled into the daemon. Decide which one you're building first — they're very different jobs.

## Choose: plugin or built-in?

| Build an **Elgato-format plugin** when… | Build a **built-in action** when… |
|---|---|
| it talks to an app, web service or device | it's a thin wrapper over a local command or system API |
| it could be useful in the official Stream Deck app too | it should cost no extra process / RAM (Node plugins are ~40–60 MB each) |
| it needs its own settings panel (HTML property inspector) | it needs deckd internals (page switching, device state, focus) |
| the user wants to keep it separate from deckd's code | it's core OS functionality (audio, shaders, clock…) |

Default to an **Elgato-format plugin** — it's isolated, hot-restartable and portable. Reach for a built-in only for the cases on the right. For built-ins, read `references/builtin-actions.md` and stop here.

## Elgato-format plugin: the workflow

### 1. Scaffold from the template

```bash
.claude/skills/deckd-plugin-dev/scripts/new-plugin.sh com.example.myplugin "My Plugin" ~/dev/plugins
```

This copies `assets/template-node/` (a working, dependency-free Node.js plugin with one key/dial action and a settings panel) and fills in the UUID and name. Read the generated `plugin.js` — it's short and commented, and shows every message type you'll need.

The template uses Node's built-in `WebSocket` (Node ≥ 22), so there is no `npm install` and no build step. If the user prefers Elgato's official TypeScript SDK (`@elgato/streamdeck`), that works too — deckd speaks the same protocol — but it needs a bundler build, and the output `bin/plugin.js` is what goes in the manifest.

### 2. Link it into deckd for development

```bash
ln -s ~/dev/plugins/com.example.myplugin.sdPlugin ~/.config/deckd/plugins/
systemctl --user restart deckd        # deckd reads manifests at startup
```

A symlink means edits apply without reinstalling. Removing the plugin from the UI deletes only the symlink, not the source.

### 3. Put the action on the deck

Open the web UI at http://127.0.0.1:28196, find the plugin in the **Actions** tab, drag the action onto a key or dial, and click it to open its settings panel in the **Inspector** tab.

### 4. The edit loop

| Changed | Do this |
|---|---|
| `plugin.js` (or other code) | `pkill -f com.example.myplugin` — deckd restarts the process within ~2 s and re-sends `willAppear` for visible actions |
| `manifest.json` (actions, icons, controllers) | `systemctl --user restart deckd` |
| property inspector HTML/JS | select a different slot and back, or reload the UI page |

Plugins start **lazily**: the process only launches when one of its actions is visible on the deck or its settings panel opens. If nothing is on screen, nothing runs — that's expected, not a bug.

### 5. Debug

- **Plugin output:** `tail -f ~/.config/deckd/logs/<plugin-uuid>.log` — stdout, stderr and crash traces.
- **deckd's side:** `journalctl --user -u deckd -f`. For every message a plugin sends, raise verbosity: `systemctl --user edit deckd` → `Environment=RUST_LOG=deckd=debug`.
- **Property inspector:** it runs inside the UI page as an iframe, so use the browser devtools on http://127.0.0.1:28196 (pick the iframe's context in the console).
- **Crash loop:** after 5 crashes within a minute deckd stops restarting the plugin (logged as "keeps crashing; giving up"). Fix, then restart deckd.

### 6. Ship it

Zip the folder so the archive contains the `<uuid>.sdPlugin/` directory at its top level, and name it `<uuid>.streamDeckPlugin`:

```bash
cd ~/dev/plugins && zip -r com.example.myplugin.streamDeckPlugin com.example.myplugin.sdPlugin -x '*/node_modules/.cache/*'
```

Install with the UI (**Plugins → Install plugin or icon pack…**, takes effect immediately) or `deckd install file.streamDeckPlugin` followed by `systemctl --user restart deckd`. Don't enable Elgato Marketplace DRM if the plugin should run in deckd — DRM-encrypted manifests (files starting with `ELGATO`) can only be read by the official app.

## Manifest essentials

deckd reads the standard Elgato manifest. The fields that matter:

```jsonc
{
  "UUID": "com.example.myplugin",       // optional; defaults to the folder name minus .sdPlugin
  "Name": "My Plugin", "Version": "1.0.0", "Author": "you",
  "Icon": "imgs/plugin",                 // image paths omit the extension
  "CodePath": "plugin.js",               // see "How deckd starts your code"
  "CodePathLin": "bin/my-linux-binary",  // optional; preferred on Linux over CodePath
  "PropertyInspectorPath": "pi/index.html",
  "SDKVersion": 2,
  "OS": [{ "Platform": "linux" }],
  "Actions": [{
    "UUID": "com.example.myplugin.hello",  // must start with the plugin UUID
    "Name": "Hello", "Tooltip": "Says hello", "Icon": "imgs/action",
    "Controllers": ["Keypad", "Encoder"],  // Keypad = key, Encoder = dial + touch strip
    "States": [{ "Image": "imgs/key", "TitleAlignment": "bottom" }],
    "Encoder": { "layout": "$B1" }        // touch-strip layout for dials
  }]
}
```

**Image paths** have no extension; deckd tries `@2x.png`, `.svg`, `.png`, `.gif`, `.jpg`. Provide `@2x.png` (144×144 for keys) or SVG for crisp results — Stream Deck+ keys are 120×120.

**How deckd starts your code** (first match wins): `CodePaths["x86_64-unknown-linux-gnu"]` → `CodePathLin` → `CodePath` → `CodePathWin`. By file type:
- `.js`/`.mjs`/`.cjs` (or a `Nodejs` key present) → run with Node (`DECKD_NODE`, else `node` on PATH, else nvm/fnm installs).
- `.html` → legacy HTML plugin, run in deckd's Node-based browser shim (DOM is stubbed; no canvas). Set `{"html_runtime": {"<uuid>": "webkit"}}` in `~/.config/deckd/settings.json` to use a real WebKit view (~250 MB) if it needs canvas or layout.
- `.exe` → Wine. Anything else → executed directly (deckd sets the executable bit).

All get Elgato's arguments: `-port 28196 -pluginUUID <uuid> -registerEvent registerPlugin -info <json>`. The info JSON reports `"platform": "linux"`.

## Protocol essentials

Connect to `ws://127.0.0.1:<port>` and send `{"event": "registerPlugin", "uuid": "<pluginUUID>"}`. Then:

- You receive `willAppear` / `willDisappear` as actions come on and off screen, `keyDown`/`keyUp`, `dialDown`/`dialUp`/`dialRotate`/`touchTap`, `didReceiveSettings`, `sendToPlugin` (from your settings panel), and others.
- You send `setTitle`, `setImage`, `setState`, `setFeedback`, `setSettings`, `showOk`/`showAlert`, `sendToPropertyInspector`, `openUrl`, `logMessage`, …

Every action instance has a stable **`context`** string — key all per-button state on it. `settings` arrive with every event; persist changes with `setSettings` (deckd saves them in the page file).

See `references/protocol.md` for exact payloads, what deckd does with each message, and the few things deckd doesn't send (e.g. `applicationDidLaunch`, `systemDidWakeUp`).

## deckd behaviours worth knowing

- **Tap/hold/turn triggers:** users can bind an action to "on tap", "on hold", "turn left/right" in the UI. Such actions receive a synthetic `keyDown`+`keyUp` pair at that moment instead of raw presses — so implement your logic on `keyUp` (or `keyDown`) without assuming timing.
- **Key-only actions on dials:** if your action lacks `"Encoder"` but the user puts it on a dial, pushing or tapping the dial sends `keyDown`/`keyUp` with `controller: "Keypad"`. Declare `"Encoder"` if you want rotation.
- **Dial drawing:** with a layout (`$X1`, `$A0`, `$A1`, `$B1`, `$B2`, `$C1` or a custom layout JSON path), update items with `setFeedback`. Alternatively, `setImage` with a **wide image (≥1.5:1, e.g. 200×100 SVG)** and deckd draws it across the whole segment — handy for fully custom strip graphics.
- **Titles** may contain `\n`; deckd draws multiple lines.
- **Images** for `setImage`: data URIs (`data:image/png;base64,…`, `data:image/svg+xml;base64,…` or `data:image/svg+xml,<svg…>`). Empty/null resets to the manifest image.
- **Global settings** live in `~/.config/deckd/plugin-data/<uuid>.json`; per-action settings live in the page files under `~/.config/deckd/profiles/`.
- **`switchToProfile`** switches to the deckd *page* with that name.

## When things don't work

| Symptom | Likely cause |
|---|---|
| Plugin missing from the Actions tab | manifest didn't parse — `journalctl --user -u deckd \| grep skipping`; or deckd wasn't restarted after linking |
| Action shows but nothing happens | the process isn't running or never registered — check its log; on Node, the `uuid` in `registerPlugin` must be the `-pluginUUID` argument |
| "no runnable code for linux" | no `CodePathLin`/`CodePath` deckd can run; Windows-only `.exe` without Wine |
| Settings panel blank | `connectElgatoStreamDeckSocket` missing or throwing in the PI — open devtools on the UI page |
| Image doesn't change | bad data URI (check `journalctl --user -u deckd \| grep image`), or the slot shows a different action from its stack |
| Install says "uses Elgato Marketplace DRM" | encrypted manifest from the Marketplace; ask for a non-DRM build |
