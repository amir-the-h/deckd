#!/usr/bin/env -S gjs -m
// Hosts one HTML-based Stream Deck plugin in an invisible WebKit view.
// Usage: DECKD_PLUGIN_ARGS='["-port","P","-pluginUUID","U",...]' html-runner.js <path/to/plugin.html>
// (Elgato's -info flag would be swallowed by gjs as -I, so arguments come via the environment.)
import GLib from "gi://GLib";
import Gtk from "gi://Gtk?version=4.0";
import WebKit from "gi://WebKit?version=6.0";
import { exit, programArgs } from "system";

const [htmlPath] = programArgs;
const rest = JSON.parse(GLib.getenv("DECKD_PLUGIN_ARGS") || "[]");
const opts = {};
for (let i = 0; i + 1 < rest.length; i += 2) opts[rest[i].replace(/^-/, "")] = rest[i + 1];

Gtk.init();
// Keep memory down: one web process, no caches, no media.
const ctx = new WebKit.WebContext();
ctx.set_cache_model(WebKit.CacheModel.DOCUMENT_VIEWER);
const settings = new WebKit.Settings({
  allow_file_access_from_file_urls: true,
  allow_universal_access_from_file_urls: true,
  enable_webgl: false,
  enable_media: false,
  enable_page_cache: false,
  enable_developer_extras: !!GLib.getenv("DECKD_DEVTOOLS"),
});
const view = new WebKit.WebView({ web_context: ctx, settings });

// Forward console output so plugin logs land in deckd's per-plugin log file.
const ucm = view.get_user_content_manager();
ucm.add_script(new WebKit.UserScript(
  `(() => { for (const k of ["log","info","warn","error","debug"]) { const o = console[k]; console[k] = (...a) => { try { window.webkit.messageHandlers.log.postMessage(k + ": " + a.map(x => typeof x === "string" ? x : JSON.stringify(x)).join(" ")); } catch {} o.apply(console, a); }; } })();`,
  WebKit.UserContentInjectedFrames.TOP_FRAME, WebKit.UserScriptInjectionTime.START, null, null));
ucm.register_script_message_handler("log", null);
ucm.connect("script-message-received::log", (_m, value) => print(value.to_string()));

view.connect("load-changed", (_v, ev) => {
  if (ev !== WebKit.LoadEvent.FINISHED) return;
  const js = `connectElgatoStreamDeckSocket(${JSON.stringify(opts.port)}, ${JSON.stringify(opts.pluginUUID)}, ${JSON.stringify(opts.registerEvent)}, ${JSON.stringify(opts.info)});`;
  view.evaluate_javascript(js, -1, null, null, null, (v, res) => {
    try { v.evaluate_javascript_finish(res); print("deckd: plugin connected"); }
    catch (e) { printerr("deckd: connectElgatoStreamDeckSocket failed: " + e.message); }
  });
});
view.connect("web-process-terminated", () => { printerr("deckd: web process died"); exit(1); });

// The view needs a toplevel to stay alive, but it is never shown.
const win = new Gtk.Window({ child: view, default_width: 1, default_height: 1 });
view.load_uri(GLib.filename_to_uri(htmlPath, null));
new GLib.MainLoop(null, false).run();
