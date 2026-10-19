# deckd

A lightweight Linux daemon for macro-key decks, with a web UI for editing pages
and actions. It was built for **Elgato Stream Deck** hardware and runs the same
Elgato-format plugins (`.sdPlugin`) the official app uses, without the official
app.

> deckd is an independent project. It is not affiliated with or endorsed by
> Elgato or Corsair. "Stream Deck" is a trademark of Corsair Memory, Inc.

## Features

- **Elgato plugin host**: runs Node.js, HTML and native `.sdPlugin` plugins over
  the Stream Deck SDK WebSocket protocol, with their property inspectors
  (settings panels) inside deckd's UI. Plugins start only when one of their
  actions is visible.
- **Web UI** at <http://127.0.0.1:28196>: a mirror of the deck at its real
  proportions, drag-and-drop pages and actions, an icon library (Iconify sets
  and `.streamDeckIconPack`s), and plugin install/removal.
- **Built-in actions** that need no plugin process: switch page, run command,
  dial commands, brightness, audio volume (PulseAudio/PipeWire), clock,
  now-playing (MPRIS) and MangoHud stats.
- **Several actions per key or dial**, each bound to a tap, a hold or a
  turn direction.
- **Automatic page switching** by focused window (class, title or "any Steam
  game") on Hyprland.
- **StreamController import**: `deckd import-streamcontroller` converts your
  pages.

Tested on a Stream Deck+; the other models supported by the
[`elgato-streamdeck`](https://crates.io/crates/elgato-streamdeck) crate should
work too.

## Requirements

- Linux, Rust 1.85+ (edition 2024) to build
- Access to the deck's hidraw node: install the udev rule below
- Node.js ≥ 22 for Node plugins (or set `DECKD_NODE`)
- Optional: `pactl` (volume), `busctl` (now playing), `hyprctl` / Hyprland
  (auto page switching), `curl` (icon set downloads), MangoHud, `gjs` with
  WebKitGTK 6 (HTML plugins that need a real browser), Wine (Windows-only
  plugins)

## Install

```bash
cargo build --release
install -Dm755 target/release/deckd ~/.local/bin/deckd

# Let your user open the deck without root
sudo install -m644 dist/70-deckd.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger

# Run it with your graphical session
install -Dm644 dist/deckd.service ~/.config/systemd/user/deckd.service
systemctl --user enable --now deckd
```

Then open <http://127.0.0.1:28196>.

## Usage

```
deckd                          run the daemon
deckd devices                  list connected decks
deckd install PATH...          install .streamDeckPlugin files or .sdPlugin folders
deckd import-streamcontroller [DIR] [--columns N]
                               import pages from StreamController
deckd preview SERIAL PAGE OUT.png
                               render a Stream Deck+ page to an image
```

Plugins can also be installed from the UI (**Plugins → Install plugin or icon
pack…**). Plugins sold on the Elgato Marketplace with DRM can't be loaded; ask
the author for a plain build.

## Configuration

Everything lives under `~/.config/deckd/` (override with `DECKD_CONFIG`):

```
settings.json                        global settings
profiles/<serial>/profile.json       per-device settings (default page, brightness)
profiles/<serial>/pages/<name>.json  one file per page
plugins/                             installed plugins (symlinks work)
plugin-data/<uuid>.json              plugins' global settings
logs/<uuid>.log                      plugin output
```

| Variable | Default | |
|---|---|---|
| `DECKD_PORT` | `28196` | HTTP/WebSocket port |
| `DECKD_CONFIG` | `~/.config/deckd` | configuration directory |
| `DECKD_NODE` | `node` on `PATH` | Node.js binary for plugins |
| `RUST_LOG` | `deckd=info` | log level |

## Security

deckd listens on `127.0.0.1` only and has no login. Anything that can open a
local connection as your user can run commands through it, the same as any
other process you run. Requests a web browser makes on behalf of other websites
are refused: the `Host` header must be a loopback name and any `Origin` must be
deckd's own UI.

## Writing plugins

Any Elgato SDK plugin works. `.claude/skills/deckd-plugin-dev/` has a guide, a
dependency-free Node template (`scripts/new-plugin.sh`), the protocol as deckd
implements it, and notes on adding built-in actions in Rust.

## License

[MIT](LICENSE)
