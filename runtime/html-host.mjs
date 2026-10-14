// Runs an HTML-based Stream Deck plugin in Node instead of a browser.
//
// Plugin pages are just a list of <script> tags whose code talks to the host
// over WebSocket; they rarely touch the DOM. We run those scripts in one V8
// context dressed up as a browser window (window, document, localStorage, ...),
// then call connectElgatoStreamDeckSocket() like the Stream Deck app does.
//
// Usage: node html-host.mjs <plugin.html> -port P -pluginUUID U -registerEvent E -info JSON
// Plugins that need real rendering (canvas, layout) should use html-runner.js (WebKit).

import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { pathToFileURL } from "node:url";

const [htmlPath, ...rest] = process.argv.slice(2);
const opts = {};
for (let i = 0; i + 1 < rest.length; i += 2) opts[rest[i].replace(/^-/, "")] = rest[i + 1];
const pageDir = path.dirname(path.resolve(htmlPath));
const pageUrl = pathToFileURL(path.resolve(htmlPath)).href;

// ---------------------------------------------------------------- DOM shim

let pageContext = null;

/** A <script src> inserted at runtime (e.g. webpack chunks): load and run it, then fire onload. */
function maybeLoadScript(el) {
  if (el?.tagName !== "SCRIPT" || !el.src || el._deckdLoaded) return;
  el._deckdLoaded = true;
  setTimeout(async () => {
    try {
      const url = new URL(String(el.src), pageUrl);
      const code = url.protocol === "file:"
        ? await fs.promises.readFile(decodeURIComponent(url.pathname), "utf8")
        : await (await fetch(url)).text();
      const prev = document.currentScript;
      document.currentScript = el;
      vm.runInContext(code, pageContext, { filename: url.href });
      document.currentScript = prev;
      const e = { type: "load", target: el };
      el.onload?.(e);
      el.dispatchEvent(e);
    } catch (err) {
      console.error(`deckd: failed to load script ${el.src}: ${err.stack || err}`);
      const e = { type: "error", target: el };
      el.onerror?.(e);
      el.dispatchEvent(e);
    }
  });
}

/** Inert element: accepts any property access or call so UI code can't crash. */
function makeElement(tag = "div") {
  const listeners = {};
  const el = {
    tagName: tag.toUpperCase(),
    nodeName: tag.toUpperCase(),
    nodeType: 1,
    style: {},
    dataset: {},
    attributes: [],
    children: [],
    childNodes: [],
    classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
    innerHTML: "",
    innerText: "",
    textContent: "",
    value: "",
    setAttribute(k, v) { this[k] = v; },
    getAttribute(k) { return this[k] ?? null; },
    removeAttribute(k) { delete this[k]; },
    hasAttribute(k) { return k in this; },
    appendChild(c) { this.children.push(c); maybeLoadScript(c); return c; },
    append(...c) { this.children.push(...c); c.forEach(maybeLoadScript); },
    prepend(...c) { this.children.unshift(...c); c.forEach(maybeLoadScript); },
    removeChild(c) { return c; },
    insertBefore(c) { this.children.push(c); maybeLoadScript(c); return c; },
    get parentNode() { return document.head; },
    replaceChildren() { this.children = []; },
    remove() {},
    cloneNode() { return makeElement(tag); },
    querySelector: () => null,
    querySelectorAll: () => [],
    getElementsByTagName: () => [],
    getElementsByClassName: () => [],
    closest: () => null,
    contains: () => false,
    getBoundingClientRect: () => ({ x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 }),
    addEventListener(t, f) { (listeners[t] ||= []).push(f); },
    removeEventListener(t, f) { listeners[t] = (listeners[t] || []).filter((x) => x !== f); },
    dispatchEvent(e) { (listeners[e.type] || []).forEach((f) => f.call(this, e)); return true; },
    focus() {}, blur() {}, click() {},
    // Canvas: report "unsupported" the way browsers without 2D context do.
    getContext: () => null,
    toDataURL: () => "data:,",
  };
  return el;
}

function makeDocument() {
  const head = makeElement("head");
  const body = makeElement("body");
  const html = makeElement("html");
  const doc = makeElement("#document");
  Object.assign(doc, {
    nodeType: 9,
    readyState: "loading",
    documentElement: html,
    head,
    body,
    title: "",
    cookie: "",
    location: null,
    currentScript: null,
    createElement: (t) => makeElement(t),
    createElementNS: (_ns, t) => makeElement(t),
    createTextNode: (t) => ({ nodeType: 3, textContent: t }),
    createDocumentFragment: () => makeElement("#fragment"),
    createEvent: () => new Event("custom"),
    getElementById: () => null,
    querySelector: (s) => (s === "head" ? head : s === "body" ? body : null),
    querySelectorAll: () => [],
    getElementsByTagName: (t) => (t === "head" ? [head] : t === "body" ? [body] : t === "html" ? [html] : []),
    getElementsByClassName: () => [],
    hasFocus: () => false,
  });
  return doc;
}

/** localStorage persisted per plugin, since some plugins keep state there. */
function makeStorage(file) {
  let data = {};
  try { data = JSON.parse(fs.readFileSync(file, "utf8")); } catch {}
  const save = () => { try { fs.mkdirSync(path.dirname(file), { recursive: true }); fs.writeFileSync(file, JSON.stringify(data)); } catch {} };
  return {
    getItem: (k) => (k in data ? data[k] : null),
    setItem: (k, v) => { data[k] = String(v); save(); },
    removeItem: (k) => { delete data[k]; save(); },
    clear: () => { data = {}; save(); },
    key: (i) => Object.keys(data)[i] ?? null,
    get length() { return Object.keys(data).length; },
  };
}

// ------------------------------------------------- networking and workers

/** Resolves a URL the way the page would see it (relative to the HTML file). */
function resolveUrl(u) {
  return new URL(String(u), pageUrl);
}

async function readLocal(url) {
  return fs.promises.readFile(decodeURIComponent(url.pathname));
}

const blobs = new Map();
let blobSeq = 0;
class PageURL extends URL {
  static createObjectURL(blob) {
    const id = `blob:deckd/${++blobSeq}`;
    blobs.set(id, blob);
    return id;
  }
  static revokeObjectURL(id) { blobs.delete(id); }
}

async function pageFetch(input, init) {
  const raw = typeof input === "string" || input instanceof URL ? String(input) : input.url;
  if (raw.startsWith("blob:")) return new Response(blobs.get(raw));
  const url = resolveUrl(raw);
  if (url.protocol === "file:") {
    try { return new Response(await readLocal(url), { status: 200 }); }
    catch { return new Response(null, { status: 404 }); }
  }
  return fetch(url, init);
}

/** Enough XMLHttpRequest for plugins that load JSON or call web APIs. */
class PageXHR {
  constructor() {
    this.readyState = 0; this.status = 0; this.responseText = ""; this.response = null;
    this.responseType = ""; this._headers = {}; this._listeners = {};
  }
  open(method, url) { this._method = method; this._url = url; this.readyState = 1; }
  setRequestHeader(k, v) { this._headers[k] = v; }
  overrideMimeType() {}
  getResponseHeader() { return null; }
  getAllResponseHeaders() { return ""; }
  addEventListener(t, f) { (this._listeners[t] ||= []).push(f); }
  removeEventListener() {}
  abort() {}
  _emit(t) {
    const e = { type: t, target: this };
    this["on" + t]?.(e);
    (this._listeners[t] || []).forEach((f) => f.call(this, e));
  }
  send(body) {
    pageFetch(this._url, { method: this._method, headers: this._headers, body })
      .then(async (res) => {
        this.status = res.status;
        this.responseText = await res.text();
        this.response = this.responseType === "json" ? JSON.parse(this.responseText || "null") : this.responseText;
        this.readyState = 4;
        this._emit("readystatechange");
        this._emit(res.ok || res.status === 0 ? "load" : "error");
        this._emit("loadend");
      })
      .catch(() => { this.readyState = 4; this._emit("readystatechange"); this._emit("error"); this._emit("loadend"); });
  }
}

/** Web Worker running in its own V8 context, talking via postMessage. */
class PageWorker {
  constructor(url) {
    this.onmessage = null;
    this._queue = [];
    this._listeners = {};
    const worker = this;
    const scope = {
      console, setTimeout, clearTimeout, setInterval, clearInterval, fetch: pageFetch,
      postMessage: (data) => setTimeout(() => worker._deliver(data)),
      addEventListener: (t, f) => { if (t === "message") scope._handlers.push(f); },
      _handlers: [],
    };
    this._scope = vm.createContext(scope);
    vm.runInContext("var self = globalThis;", this._scope);
    pageFetch(url).then((r) => r.text()).then((code) => {
      vm.runInContext(code, this._scope, { filename: String(url) });
      this._ready = true;
      this._queue.splice(0).forEach((d) => this.postMessage(d));
    }).catch((e) => console.error("deckd: worker failed:", e));
  }
  _deliver(data) {
    const e = { data, target: this };
    this.onmessage?.(e);
    (this._listeners.message || []).forEach((f) => f(e));
  }
  postMessage(data) {
    if (!this._ready) return this._queue.push(data);
    const e = { data: structuredClone(data) };
    setTimeout(() => {
      this._scope.onmessage?.(e);
      this._scope._handlers.forEach((f) => f(e));
    });
  }
  addEventListener(t, f) { (this._listeners[t] ||= []).push(f); }
  removeEventListener() {}
  terminate() { this._ready = false; }
}

// ------------------------------------------------------------- the window

const document = makeDocument();
const location = new URL(pageUrl);
const storageFile = path.join(process.env.DECKD_PLUGIN_DATA || pageDir, `${opts.pluginUUID || "plugin"}.localStorage.json`);
const windowListeners = {};

const sandbox = {
  console,
  setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, queueMicrotask,
  fetch: pageFetch, Headers, Request, Response, FormData, AbortController, AbortSignal,
  WebSocket, URL: PageURL, URLSearchParams, Worker: PageWorker, TextEncoder, TextDecoder, Blob, atob, btoa,
  crypto: globalThis.crypto, structuredClone, performance,
  Event, EventTarget, CustomEvent: globalThis.CustomEvent ?? class CustomEvent extends Event {
    constructor(t, o = {}) { super(t, o); this.detail = o.detail; }
  },
  MessageChannel, BroadcastChannel,
  document,
  location,
  navigator: { userAgent: "Mozilla/5.0 (X11; Linux x86_64) deckd", language: "en-US", languages: ["en-US"], platform: "Linux x86_64", onLine: true, clipboard: { writeText: async () => {} } },
  localStorage: makeStorage(storageFile),
  sessionStorage: makeStorage(path.join("/tmp", `deckd-${process.pid}-session.json`)),
  Image: class { constructor() { this.onload = null; this.onerror = null; } set src(v) { this._src = v; setTimeout(() => this.onerror?.(new Event("error"))); } get src() { return this._src; } },
  Audio: class { play() { return Promise.resolve(); } pause() {} },
  XMLHttpRequest: PageXHR,
  requestAnimationFrame: (f) => setTimeout(() => f(performance.now()), 16),
  cancelAnimationFrame: (id) => clearTimeout(id),
  getComputedStyle: () => ({ getPropertyValue: () => "" }),
  matchMedia: () => ({ matches: false, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {} }),
  alert: (m) => console.log("alert:", m),
  confirm: () => false,
  prompt: () => null,
  open: (url) => { console.log("window.open:", url); return null; },
  addEventListener: (t, f) => { (windowListeners[t] ||= []).push(f); },
  removeEventListener: (t, f) => { windowListeners[t] = (windowListeners[t] || []).filter((x) => x !== f); },
  dispatchEvent: (e) => { (windowListeners[e.type] || []).forEach((f) => f(e)); return true; },
  innerWidth: 0, innerHeight: 0, devicePixelRatio: 2,
};
document.location = location;
const context = vm.createContext(sandbox);
pageContext = context;
// Make window/self/globalThis all point at the sandbox global, as in a browser.
vm.runInContext("var window = globalThis, self = globalThis, top = globalThis, parent = globalThis;", context);

// ------------------------------------------------------------ run the page

const html = fs.readFileSync(htmlPath, "utf8");
const scripts = [];
for (const m of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
  const attrs = m[1];
  const src = attrs.match(/\bsrc\s*=\s*["']([^"']+)["']/i)?.[1];
  if (/\btype\s*=\s*["']module["']/i.test(attrs)) {
    console.warn(`deckd: module scripts are not supported (${src || "inline"})`);
    continue;
  }
  if (src && /^https?:/i.test(src)) {
    console.warn(`deckd: skipping remote script ${src}`);
    continue;
  }
  scripts.push(src ? { file: path.resolve(pageDir, decodeURI(src.split("?")[0])) } : { code: m[2], file: `${htmlPath}#inline` });
}

for (const s of scripts) {
  const code = s.code ?? fs.readFileSync(s.file, "utf8");
  document.currentScript = { src: pathToFileURL(s.file).href, getAttribute: () => null };
  try {
    vm.runInContext(code, context, { filename: s.file });
  } catch (e) {
    console.error(`deckd: error in ${path.relative(pageDir, s.file)}: ${e.stack || e}`);
  }
}
document.currentScript = null;
document.readyState = "complete";
document.dispatchEvent(new Event("DOMContentLoaded"));
sandbox.dispatchEvent(new Event("load"));

const connect = context.connectElgatoStreamDeckSocket;
if (typeof connect !== "function") {
  console.error("deckd: page did not define connectElgatoStreamDeckSocket");
  process.exit(1);
}
connect(opts.port, opts.pluginUUID, opts.registerEvent, opts.info);
console.log("deckd: plugin connected");
