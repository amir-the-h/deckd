"use strict";
// deckd web UI. Talks to the daemon's JSON API; previews are rendered by the
// daemon itself so they match the hardware exactly.

const $ = (sel, root = document) => root.querySelector(sel);
const el = (tag, attrs = {}, ...children) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (k === "html") e.innerHTML = v;
    else e.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat()) if (c !== null && c !== undefined && c !== false) e.append(c.nodeType ? c : document.createTextNode(c));
  return e;
};

const S = {
  devices: [],
  serial: null,
  dev: null,
  snap: null,
  page: null,
  catalog: [],
  actions: {},     // uuid -> catalog entry (+ pluginName)
  sel: null,       // "key-3" | "dial-1"
  selAction: 0,
  tick: {},        // input -> cache-busting counter
  pi: null,        // { context, frame, timer }
  adding: false,
  audio: [],
  icons: [],
};

// ------------------------------------------------------------------ helpers

async function api(method, url, body) {
  const opts = { method, headers: {} };
  if (body instanceof Blob || body instanceof ArrayBuffer) {
    opts.body = body;
  } else if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error((await res.text()) || res.statusText);
  const type = res.headers.get("content-type") || "";
  return type.includes("json") ? res.json() : null;
}

function toast(msg, error = false) {
  const t = $("#toast");
  t.textContent = msg;
  t.className = "toast" + (error ? " error" : "");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => t.classList.add("hidden"), 3000);
}

function modal(title, value = "", withInput = true) {
  return new Promise((resolve) => {
    const m = $("#modal"), input = $("#modal-input");
    $("#modal-title").textContent = title;
    input.classList.toggle("hidden", !withInput);
    input.value = value;
    m.classList.remove("hidden");
    if (withInput) setTimeout(() => { input.focus(); input.select(); });
    const done = (v) => {
      m.classList.add("hidden");
      $("#modal-form").onsubmit = null;
      $("#modal-cancel").onclick = null;
      resolve(v);
    };
    $("#modal-form").onsubmit = (e) => { e.preventDefault(); done(withInput ? input.value.trim() : true); };
    $("#modal-cancel").onclick = () => done(null);
  });
}

const debounce = (fn, ms) => {
  let t;
  return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
};

const pageObj = () => S.snap?.pages?.[S.page];
const isLivePage = () => S.snap && S.page === S.snap.current;

function slotKey(input) {
  const [kind, n] = input.split("-");
  return [kind === "key" ? "keys" : "dials", n];
}
function getSlot(input) {
  const [sect, n] = slotKey(input);
  return pageObj()?.[sect]?.[n];
}
function setSlot(input, slot) {
  const page = pageObj();
  const [sect, n] = slotKey(input);
  page[sect] ||= {};
  if (!slot || !slot.actions?.length) delete page[sect][n];
  else page[sect][n] = slot;
}
function curAction() {
  return getSlot(S.sel)?.actions?.[S.selAction];
}
function actionMeta(uuid) {
  return S.actions[uuid] || {
    uuid, name: uuid.replace(/^sc:/, "").split("::").pop(), pluginName: uuid.startsWith("sc:") ? "StreamController (not available)" : "Unknown plugin",
    missing: true, keypad: true, encoder: true,
  };
}
function inputLabel(input) {
  const [kind, n] = input.split("-");
  if (kind === "dial") return `Dial ${+n + 1}`;
  const cols = S.dev.columns;
  return `Key ${+n + 1}  ·  row ${Math.floor(n / cols) + 1}, column ${(n % cols) + 1}`;
}
const imgUrl = (src) => !src ? null : (src.startsWith("data:") || src.startsWith("/") && !src.startsWith("/home") ? src : `/files?path=${encodeURIComponent(src)}`);

// ------------------------------------------------------------------ loading

async function loadDevices() {
  S.devices = await api("GET", "/api/devices");
  const sel = $("#device");
  sel.innerHTML = "";
  for (const d of S.devices) sel.append(el("option", { value: d.serial }, `Stream Deck ${d.kind}  (${d.serial})`));
  if (!S.devices.length) {
    sel.append(el("option", {}, "No Stream Deck connected"));
    S.serial = null;
    S.dev = null;
    renderAll();
    return;
  }
  if (!S.devices.some((d) => d.serial === S.serial)) S.serial = S.devices[0].serial;
  sel.value = S.serial;
  S.dev = S.devices.find((d) => d.serial === S.serial);
  await loadSnapshot();
}

async function loadSnapshot() {
  if (!S.serial) return;
  S.snap = await api("GET", `/api/devices/${S.serial}`);
  if (!S.page || !S.snap.pages[S.page]) S.page = S.snap.current;
  if (S.sel && !getSlot(S.sel)) S.selAction = 0;
  renderAll();
}

async function loadCatalog() {
  S.catalog = await api("GET", "/api/actions");
  S.actions = {};
  for (const g of S.catalog) for (const a of g.actions) S.actions[a.uuid] = { ...a, pluginName: g.name, plugin: g.plugin };
  renderLibrary();
}

async function savePage() {
  const name = S.page;
  S.pendingSave = false;
  try {
    const page = await api("PUT", `/api/devices/${S.serial}/pages/${encodeURIComponent(name)}`, pageObj());
    S.snap.pages[name] = page;
  } catch (e) {
    toast(`Couldn't save: ${e.message}`, true);
  }
}
const savePageDebounced = debounce(savePage, 350);
// Marks the current page as having unsaved edits so a reload in the meantime doesn't drop them.
const savePageSoon = () => {
  S.pendingSave = true;
  savePageDebounced();
};

// ------------------------------------------------------------------ events

function connectEvents() {
  const ws = new WebSocket(`ws://${location.host}/api/events`);
  const status = $("#status");
  ws.onopen = () => { status.textContent = "connected"; status.className = "status ok"; };
  ws.onclose = () => {
    status.textContent = "daemon not reachable";
    status.className = "status down";
    setTimeout(connectEvents, 2000);
  };
  ws.onmessage = (m) => {
    const ev = JSON.parse(m.data);
    if (ev.type === "devices") return loadDevices();
    if (ev.type === "plugins") {
      loadCatalog();
      if (!$("#tab-plugins").classList.contains("hidden")) renderPlugins();
      return;
    }
    if (ev.serial !== S.serial) return;
    if (ev.type === "render" && isLivePage()) refreshSlot(ev.input);
    else if (ev.type === "page") {
      S.snap.current = ev.page;
      renderPages();
      renderStageHead();
    } else if (ev.type === "config" || ev.type === "settings") {
      reloadQuietly();
    }
  };
}

// Refetch without disturbing inputs being edited or an open settings panel.
const reloadQuietly = debounce(async () => {
  const fresh = await api("GET", `/api/devices/${S.serial}`);
  if (S.pendingSave && S.snap?.pages?.[S.page]) fresh.pages[S.page] = S.snap.pages[S.page];
  S.snap = fresh;
  if (!S.snap.pages[S.page]) S.page = S.snap.current;
  renderPages();
  renderStageHead();
  if (!$("#page-settings").contains(document.activeElement)) renderPageSettings();
  refreshAllSlots();
  renderStack();
}, 150);

// ------------------------------------------------------------------ pages

function renderPages() {
  const list = $("#page-list");
  list.innerHTML = "";
  if (!S.snap) return;
  for (const name of Object.keys(S.snap.pages)) {
    const li = el("li", {
      class: [name === S.page && "selected", name === S.snap.current && "live"].filter(Boolean).join(" "),
      title: name === S.snap.current ? "Showing on the deck" : "",
      onclick: () => selectPage(name),
      ondblclick: () => renamePage(name),
    }, el("span", { class: "dot" }), el("span", {}, name), name === S.snap.profile.default_page ? el("span", { class: "star", title: "Default page" }, "★") : null);
    list.append(li);
  }
  $("#brightness").value = S.snap.profile.brightness;
  $("#brightness-value").textContent = `${S.snap.profile.brightness}%`;
}

function selectPage(name) {
  S.page = name;
  S.sel = null;
  S.selAction = 0;
  unmountPI();
  renderAll();
}

// Escapes text so a window class can be used literally in a regex.
const reEscape = (t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

function renderPageSettings() {
  const box = $("#page-settings");
  box.innerHTML = "";
  const page = pageObj();
  if (!page) return;
  const rule = page.auto || {};
  const enabled = !!page.auto && rule.enabled !== false;
  const save = () => savePageSoon();
  // Look the page up on every change: the snapshot is replaced whenever deckd reports an update.
  const setRule = (patch) => {
    const p = pageObj();
    if (!p) return;
    p.auto = { enabled: true, ...p.auto, ...patch };
    save();
  };

  const toggle = el("input", { type: "checkbox", checked: enabled ? true : undefined });
  const cls = el("input", { value: rule.class || "", placeholder: "e.g. obs or chrome-app\\.zoom\\.us" });
  const title = el("input", { value: rule.title || "", placeholder: "optional, e.g. Zoom Meeting" });
  const back = el("input", { type: "checkbox", checked: rule.stay ? undefined : true });
  const fields = el("div", { class: "stack" + (enabled ? "" : " hidden") });

  toggle.addEventListener("change", () => {
    if (toggle.checked) setRule({ enabled: true });
    else if (pageObj()?.auto) { pageObj().auto.enabled = false; save(); }
    fields.classList.toggle("hidden", !toggle.checked);
  });
  cls.addEventListener("input", () => setRule({ class: cls.value.trim() }));
  title.addEventListener("input", () => setRule({ title: title.value.trim() }));
  back.addEventListener("change", () => setRule({ stay: !back.checked }));

  const list = el("div", { class: "window-list hidden" });
  const pick = el("button", { class: "btn small", style: "flex:none" }, "Pick from open windows…");
  pick.addEventListener("click", async () => {
    if (!list.classList.contains("hidden")) return list.classList.add("hidden");
    const wins = await api("GET", "/api/windows").catch(() => []);
    list.innerHTML = "";
    if (!wins.length) list.append(el("p", { class: "muted" }, "No windows found (is Hyprland running?)."));
    for (const w of wins) {
      list.append(el("button", {
        onclick: () => { cls.value = `^${reEscape(w.class)}$`; setRule({ class: cls.value }); list.classList.add("hidden"); },
      }, el("span", { class: "cls" }, w.class), el("span", { class: "ttl" }, w.title.slice(0, 60))));
    }
    list.classList.remove("hidden");
  });

  fields.append(
    el("div", { class: "row" }, field("Window class", cls), pick),
    list,
    field("Window title", title),
    el("label", { class: "row", style: "justify-content:flex-start;gap:8px" }, back, el("span", { style: "flex:none" }, "Go back to the previous page when the app loses focus")),
    el("p", { class: "muted" }, "Patterns are case-insensitive regular expressions; a plain word matches anywhere in the name. Changing pages by hand while on an auto page cancels the automatic return."));
  box.append(
    el("div", { class: "section-head" }, el("h2", {}, "Page settings")),
    el("label", { class: "row", style: "justify-content:flex-start;gap:8px" }, toggle, el("span", { style: "flex:none" }, "Switch to this page automatically when an app is focused")),
    fields);
}

function renderStageHead() {
  $("#page-title").textContent = S.page || "—";
  const live = isLivePage();
  $("#on-deck").classList.toggle("hidden", !live);
  $("#show-on-deck").classList.toggle("hidden", live);
  $("#page-default").disabled = S.snap?.profile.default_page === S.page;
}

async function addPage() {
  const name = await modal("New page name", "");
  if (!name) return;
  if (S.snap.pages[name]) return toast("A page with that name already exists", true);
  try {
    S.snap.pages[name] = await api("PUT", `/api/devices/${S.serial}/pages/${encodeURIComponent(name)}`, {});
    selectPage(name);
  } catch (e) { toast(e.message, true); }
}

async function renamePage(from = S.page) {
  const to = await modal(`Rename "${from}"`, from);
  if (!to || to === from) return;
  try {
    await api("POST", `/api/devices/${S.serial}/pages/${encodeURIComponent(from)}/rename`, { to });
    if (S.page === from) S.page = to;
    await loadSnapshot();
  } catch (e) { toast(e.message, true); }
}

async function deletePage() {
  const ok = await modal(`Delete page "${S.page}"? This can't be undone.`, "", false);
  if (!ok) return;
  try {
    await api("DELETE", `/api/devices/${S.serial}/pages/${encodeURIComponent(S.page)}`);
    S.page = null;
    S.sel = null;
    unmountPI();
    await loadSnapshot();
  } catch (e) { toast(e.message, true); }
}

async function setProfile(patch) {
  const profile = { ...S.snap.profile, ...patch };
  try {
    await api("PUT", `/api/devices/${S.serial}/profile`, profile);
    S.snap.profile = profile;
    renderPages();
    renderStageHead();
  } catch (e) { toast(e.message, true); }
}

// ------------------------------------------------------------------ deck

function slotSrc(input) {
  return `/api/devices/${S.serial}/render/${encodeURIComponent(S.page)}/${input}?v=${S.tick[input] || 0}`;
}

function refreshSlot(input) {
  S.tick[input] = (S.tick[input] || 0) + 1;
  const node = document.querySelector(`.slot[data-input="${input}"]`);
  if (!node) return;
  $("img", node).src = slotSrc(input);
  node.classList.toggle("empty", !getSlot(input));
}

function refreshAllSlots() {
  document.querySelectorAll(".slot").forEach((n) => refreshSlot(n.dataset.input));
}

function makeSlot(input, cls) {
  const node = el("div", {
    class: `slot ${cls}${getSlot(input) ? "" : " empty"}${S.sel === input ? " selected" : ""}`,
    "data-input": input,
    draggable: "true",
    onclick: () => select(input),
  }, el("img", { src: slotSrc(input), alt: "" }));

  node.addEventListener("dragstart", (e) => {
    if (!getSlot(input)) return e.preventDefault();
    e.dataTransfer.setData("application/x-deckd-slot", input);
    e.dataTransfer.effectAllowed = "move";
  });
  node.addEventListener("dragover", (e) => {
    const types = e.dataTransfer.types;
    if (types.includes("application/x-deckd-action") || types.includes("application/x-deckd-slot")) {
      e.preventDefault();
      node.classList.add("drop");
    }
  });
  node.addEventListener("dragleave", () => node.classList.remove("drop"));
  node.addEventListener("drop", (e) => {
    e.preventDefault();
    node.classList.remove("drop");
    const uuid = e.dataTransfer.getData("application/x-deckd-action");
    const from = e.dataTransfer.getData("application/x-deckd-slot");
    if (uuid) placeAction(input, uuid);
    else if (from && from !== input) moveSlot(from, input);
  });
  return node;
}

function renderDeck() {
  const deck = $("#deck");
  deck.innerHTML = "";
  if (!S.dev || !S.snap) {
    deck.append(el("p", { class: "muted" }, "Waiting for a Stream Deck…"));
    return;
  }
  // Lay the mirror out in the device's real proportions. On a Stream Deck+ each
  // key column sits over one 200x100 touch-strip segment, and the 120px key
  // image covers ~60% of that column, so everything derives from the column pitch.
  const cols = S.dev.columns;
  const stage = $(".stage").clientWidth - 120;
  const pitch = Math.max(100, Math.min(190, Math.floor(stage / cols)));
  const hasStrip = S.dev.dials > 0 && S.dev.lcd;
  const keyPx = Math.round(hasStrip ? pitch * (S.dev.keySize / (S.dev.lcd[0] / cols)) : pitch * 0.8);
  const width = pitch * cols;

  const keys = el("div", { class: "keys" });
  keys.style.gridTemplateColumns = `repeat(${cols}, ${pitch}px)`;
  keys.style.rowGap = `${Math.round(pitch - keyPx) * 0.55}px`;
  for (let k = 0; k < S.dev.keys; k++) {
    const slot = makeSlot(`key-${k}`, "key");
    slot.style.width = slot.style.height = `${keyPx}px`;
    slot.style.borderRadius = `${Math.round(keyPx * 0.14)}px`;
    keys.append(slot);
  }
  deck.append(keys);

  if (S.dev.dials > 0) {
    if (hasStrip) {
      const strip = el("div", { class: "strip" });
      strip.style.gridTemplateColumns = `repeat(${S.dev.dials}, 1fr)`;
      strip.style.width = `${width}px`;
      for (let d = 0; d < S.dev.dials; d++) strip.append(makeSlot(`dial-${d}`, "segment"));
      deck.append(strip);
    }
    const knobs = el("div", { class: "knobs" });
    knobs.style.gridTemplateColumns = `repeat(${S.dev.dials}, 1fr)`;
    knobs.style.width = `${width}px`;
    const knobPx = Math.round((width / S.dev.dials) * 0.5);
    for (let d = 0; d < S.dev.dials; d++) {
      const knob = el("div", { class: `knob${S.sel === `dial-${d}` ? " selected" : ""}`, title: `Dial ${d + 1}`, onclick: () => select(`dial-${d}`) });
      knob.style.width = knob.style.height = `${knobPx}px`;
      knobs.append(knob);
    }
    deck.append(knobs);
  }
}

function select(input) {
  if (S.sel !== input) {
    S.selAction = Math.max(0, getSlot(input)?.display ?? 0);
    unmountPI();
  }
  S.sel = input;
  document.querySelectorAll(".slot, .knob").forEach((n) => n.classList.remove("selected"));
  document.querySelector(`.slot[data-input="${input}"]`)?.classList.add("selected");
  if (input.startsWith("dial")) document.querySelectorAll(".knob")[+input.split("-")[1]]?.classList.add("selected");
  showTab("inspector");
  renderInspector();
}

function canPlace(input, uuid) {
  const meta = actionMeta(uuid);
  if (input.startsWith("key") && !meta.keypad) return "That action only works on a dial.";
  return null;
}

function newInstance(uuid) {
  return { uuid, settings: {} };
}

async function placeAction(input, uuid) {
  const problem = canPlace(input, uuid);
  if (problem) return toast(problem, true);
  setSlot(input, { actions: [newInstance(uuid)] });
  S.sel = null;
  await savePage();
  refreshSlot(input);
  select(input);
}

async function appendAction(uuid) {
  const problem = canPlace(S.sel, uuid);
  if (problem) return toast(problem, true);
  const slot = getSlot(S.sel) || { actions: [] };
  slot.actions.push(newInstance(uuid));
  setSlot(S.sel, slot);
  S.selAction = slot.actions.length - 1;
  unmountPI();
  await savePage();
  refreshSlot(S.sel);
  renderInspector();
}

async function moveSlot(from, to) {
  const a = getSlot(from), b = getSlot(to);
  for (const act of a?.actions || []) {
    const p = canPlace(to, act.uuid);
    if (p) return toast(p, true);
  }
  for (const act of b?.actions || []) {
    const p = canPlace(from, act.uuid);
    if (p) return toast(p, true);
  }
  setSlot(to, a);
  setSlot(from, b);
  await savePage();
  refreshSlot(from);
  refreshSlot(to);
  select(to);
}

async function clearSlot() {
  if (!S.sel) return;
  unmountPI();
  setSlot(S.sel, null);
  await savePage();
  refreshSlot(S.sel);
  renderInspector();
}

// ------------------------------------------------------------------ library

function renderLibrary() {
  const lib = $("#library");
  const q = $("#search").value.trim().toLowerCase();
  lib.innerHTML = "";
  lib.classList.toggle("adding", S.adding);
  if (S.adding) {
    lib.append(el("div", { class: "section" },
      el("div", {}, `Click an action to add it to ${inputLabel(S.sel)}.`),
      el("button", { class: "btn small", onclick: () => { S.adding = false; renderLibrary(); showTab("inspector"); } }, "Cancel")));
  }
  for (const g of S.catalog) {
    const items = g.actions.filter((a) => !q || a.name.toLowerCase().includes(q) || g.name.toLowerCase().includes(q) || (a.tooltip || "").toLowerCase().includes(q));
    if (!items.length) continue;
    const details = el("details", { class: "group", open: q || g.plugin === "deckd" ? true : undefined },
      el("summary", {},
        g.icon ? el("img", { src: g.icon, alt: "" }) : null,
        el("span", {}, g.name),
        g.runnable === false ? el("span", { class: "warn" }, "can't run on Linux") : null,
        el("span", { class: "count" }, items.length)));
    for (const a of items) {
      const tags = [a.keypad && "key", a.encoder && "dial"].filter(Boolean).join(" · ");
      const item = el("div", {
        class: "action-item", draggable: "true", title: a.tooltip || a.name,
        onclick: () => {
          if (S.adding) {
            S.adding = false;
            showTab("inspector");
            appendAction(a.uuid);
          }
        },
      },
        a.icon ? el("img", { src: a.icon, alt: "" }) : el("div", { class: "noicon" }),
        el("div", { class: "meta" }, el("span", {}, a.name), el("span", { class: "tip" }, a.tooltip || ""), el("span", { class: "tags" }, tags)));
      item.addEventListener("dragstart", (e) => {
        e.dataTransfer.setData("application/x-deckd-action", a.uuid);
        e.dataTransfer.effectAllowed = "copy";
      });
      details.append(item);
    }
    lib.append(details);
  }
}

function showTab(name) {
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("active", t.dataset.tab === name));
  $("#tab-library").classList.toggle("hidden", name !== "library");
  $("#tab-inspector").classList.toggle("hidden", name !== "inspector");
  $("#tab-plugins").classList.toggle("hidden", name !== "plugins");
  if (name === "plugins") renderPlugins();
}

// ------------------------------------------------------------------ inspector

function renderInspector() {
  const root = $("#inspector");
  // Keep an open settings panel alive across re-renders; moving an iframe reloads it.
  const keepPI = S.pi && curAction()?.context === S.pi.context ? S.pi.frame : null;
  if (keepPI) keepPI.remove();
  root.innerHTML = "";
  if (!S.sel || !S.snap) {
    root.append(el("p", { class: "empty" }, "Select a key or dial."));
    return;
  }
  const slot = getSlot(S.sel);
  root.append(el("div", { class: "section-head row" },
    el("h3", {}, inputLabel(S.sel)),
    slot ? el("button", { class: "btn small danger", style: "flex:none", onclick: clearSlot }, "Clear") : null));

  if (!slot) {
    root.append(el("div", { class: "section" },
      el("p", { class: "muted" }, "Empty. Drag an action here from the Actions tab, or:"),
      el("button", { class: "btn", onclick: startAdding }, "+ Add action")));
    return;
  }
  if (S.selAction >= slot.actions.length) S.selAction = 0;

  root.append(el("div", { id: "stack-section", class: "section" }));
  renderStack();

  const settings = el("div", { class: "section" }, el("div", { class: "section-head" }, el("h2", {}, "Settings")));
  renderSettings(settings, keepPI);
  root.append(settings);

  root.append(renderAppearance(slot));
}

function renderStack() {
  const section = $("#stack-section");
  const slot = S.sel && getSlot(S.sel);
  if (!section || !slot) return;
  section.innerHTML = "";
  const multi = slot.actions.length > 1;
  section.append(el("div", { class: "section-head" },
    el("h2", {}, multi ? "Actions" : "Action"),
    el("button", { class: "btn small", onclick: startAdding, title: "Add another action to this slot" }, "+ Add")));
  const stack = el("div", { class: "stack" });
  slot.actions.forEach((a, i) => {
    const meta = actionMeta(a.uuid);
    const isDial = S.sel.startsWith("dial");
    const trig = el("select", {
      title: "When this action fires",
      onclick: (e) => e.stopPropagation(),
      onchange: (e) => {
        const v = e.target.value;
        const act = getSlot(S.sel)?.actions[i];
        if (!act) return;
        if (v) act.trigger = v; else delete act.trigger;
        savePage();
      },
    },
      el("option", { value: "" }, "Default"),
      el("option", { value: "down" }, "On press"),
      el("option", { value: "tap" }, "On tap"),
      el("option", { value: "hold" }, "On hold"),
      isDial ? el("option", { value: "cw" }, "Turn right") : null,
      isDial ? el("option", { value: "ccw" }, "Turn left") : null);
    trig.value = a.trigger || "";
    const controls = el("div", { class: "controls" },
      multi ? trig : null,
      multi ? el("button", {
        class: "btn small", title: "Show this action's picture on the deck",
        onclick: (e) => { e.stopPropagation(); getSlot(S.sel).display = i; savePage().then(() => { refreshSlot(S.sel); renderInspector(); }); },
      }, (slot.display || 0) === i ? "◉" : "○") : null,
      el("button", {
        class: "btn small", title: "Remove",
        onclick: async (e) => {
          e.stopPropagation();
          const cur = getSlot(S.sel);
          cur.actions.splice(i, 1);
          if ((cur.display || 0) >= cur.actions.length) cur.display = 0;
          setSlot(S.sel, cur.actions.length ? cur : null);
          S.selAction = 0;
          unmountPI();
          await savePage();
          refreshSlot(S.sel);
          renderInspector();
        },
      }, "×"));
    stack.append(el("div", {
      class: `stack-item${i === S.selAction ? " active" : ""}`,
      onclick: () => { if (S.selAction !== i) { S.selAction = i; unmountPI(); renderInspector(); } },
    },
      meta.icon ? el("img", { src: meta.icon, alt: "" }) : el("img", { alt: "" }),
      el("div", {}, el("div", { class: "name" }, meta.name), el("div", { class: "sub" }, meta.pluginName)),
      controls));
  });
  section.append(stack);
  if (!multi) {
    section.append(el("p", { class: "muted" }, "Add more actions to run several from one slot, e.g. one on tap and another on hold."));
  }
}

function startAdding() {
  S.adding = true;
  renderLibrary();
  showTab("library");
}

function field(label, input) {
  return el("label", { class: "field" }, el("span", {}, label), input);
}

function settingInput(a, key, attrs = {}) {
  const input = el("input", { value: a.settings?.[key] ?? "", ...attrs });
  input.addEventListener("input", () => {
    const act = curAction();
    act.settings ||= {};
    const v = attrs.type === "number" ? (input.value === "" ? undefined : Number(input.value)) : input.value;
    if (v === undefined || v === "") delete act.settings[key]; else act.settings[key] = v;
    savePageSoon();
  });
  return input;
}

function settingCheckbox(a, key, label, def) {
  const box = el("input", { type: "checkbox", checked: (a.settings?.[key] ?? def) ? true : undefined });
  box.addEventListener("change", () => {
    const act = curAction();
    act.settings ||= {};
    act.settings[key] = box.checked;
    savePage().then(() => refreshSlot(S.sel));
  });
  return el("label", { class: "row", style: "justify-content:flex-start;gap:8px" }, box, el("span", { style: "flex:none" }, label));
}

function settingSelect(a, key, options, onChange) {
  const sel = el("select", {}, options.map(([v, label]) => el("option", { value: v }, label)));
  sel.value = a.settings?.[key] ?? options[0][0];
  sel.addEventListener("change", () => {
    const act = curAction();
    act.settings ||= {};
    if (sel.value === "") delete act.settings[key]; else act.settings[key] = sel.value;
    savePage();
    onChange?.(sel.value);
  });
  return sel;
}

function renderSettings(box, keepPI) {
  const a = curAction();
  const meta = actionMeta(a.uuid);
  if (meta.missing) {
    box.append(el("p", { class: "muted" },
      a.uuid.startsWith("sc:")
        ? "This came from StreamController and has no equivalent installed. Replace it with an action from the library."
        : "The plugin for this action isn't installed."));
  } else if (a.uuid === "deckd.page") {
    const pages = Object.keys(S.snap.pages).map((p) => [p, p]);
    box.append(field("Go to", settingSelect(a, "page", [["", "Choose a page…"], ["@back", "← Previous page"], ...pages])));
  } else if (a.uuid === "deckd.command") {
    box.append(field("Command", settingInput(a, "command", { placeholder: "e.g. firefox, or notify-send hi" })));
    box.append(el("p", { class: "muted" }, "Runs with sh -c, detached from deckd."));
  } else if (a.uuid === "deckd.dial") {
    for (const [k, l] of [["cw", "Turn right"], ["ccw", "Turn left"], ["press", "Push"], ["touch", "Tap strip"], ["long_touch", "Long-press strip"]]) {
      box.append(field(l, settingInput(a, k, { placeholder: "command" })));
    }
    box.append(el("p", { class: "muted" }, "{ticks} is replaced with how far the dial turned."));
  } else if (a.uuid === "deckd.brightness") {
    box.append(field("Change per press (%)", settingInput(a, "delta", { type: "number", placeholder: "10" })));
    box.append(el("p", { class: "muted" }, "On a dial, turning adjusts brightness."));
  } else if (a.uuid === "deckd.volume") {
    renderVolumeSettings(box, a);
  } else if (a.uuid === "deckd.shader") {
    renderShaderSettings(box, a);
  } else if (a.uuid === "deckd.clock") {
    const analog = a.settings?.style === "analog";
    box.append(field("Style", settingSelect(a, "style", [["digital", "Digital"], ["analog", "Analog"]], () => { renderInspector(); refreshSlot(S.sel); })));
    if (!analog) {
      box.append(field("Format", settingInput(a, "format", { placeholder: "%H:%M" })));
      box.append(el("p", { class: "muted" }, "strftime codes: %H:%M → 14:05 · %I:%M %p → 02:05 PM · %d / %m → 15 / 10 · %a %d %b → Thu 15 Oct · \n for a new line. Style the text with the Center label in Appearance."));
    }
  } else if (a.uuid === "deckd.media") {
    box.append(field("Player", settingInput(a, "player", { placeholder: "Any player (or e.g. spotify, chromium, firefox)" })));
    box.append(settingCheckbox(a, "art", "Show cover art", true));
    box.append(settingCheckbox(a, "title", "Show the track title", true));
    box.append(el("p", { class: "muted" }, "Press or tap to play/pause. On a dial, turn to skip tracks."));
  } else if (a.uuid === "deckd.mangohud") {
    const stat = a.settings?.mode !== "toggle";
    box.append(field("Mode", settingSelect(a, "mode", [["stat", "Show a live stat"], ["toggle", "Toggle the HUD"]], () => { renderInspector(); refreshSlot(S.sel); })));
    if (stat) {
      const fields = (actionMeta("deckd.mangohud").fields || ["fps"]).map((f) => [f, f.replace(/_/g, " ")]);
      const positions = [["top", "Top"], ["center", "Middle"], ["bottom", "Bottom"]];
      box.append(field("Stat", settingSelect(a, "field", fields)));
      box.append(el("div", { class: "row" }, field("Label", settingInput(a, "label", { placeholder: "e.g. FPS" })), field("Label at", settingSelect(a, "label_pos", positions))));
      box.append(field("Value at", settingSelect(a, "stat_pos", [["center", "Middle"], ["top", "Top"], ["bottom", "Bottom"]])));
    }
    box.append(el("p", { class: "muted" }, "Needs MangoHud's control socket and logging enabled in MangoHud.conf (control=…/scmango-%p, output_folder=/tmp/sc_mangohud, autostart_log=1)."));
  } else if (meta.inspector) {
    const holder = el("div");
    box.append(holder);
    mountPI(holder, a.context, keepPI);
  } else if (!a.uuid.startsWith("deckd.")) {
    box.append(el("p", { class: "muted" }, "This action has no settings."));
  }

  if (!a.uuid.startsWith("deckd.") || meta.missing) {
    const ta = el("textarea", { spellcheck: "false" });
    ta.value = JSON.stringify(a.settings ?? {}, null, 2);
    ta.addEventListener("change", () => {
      try {
        curAction().settings = JSON.parse(ta.value || "{}");
        savePage();
      } catch { toast("Settings must be valid JSON", true); }
    });
    box.append(el("details", { class: "raw" }, el("summary", {}, "Raw settings (JSON)"), ta));
  }
}

async function renderShaderSettings(box, a) {
  const mode = a.settings?.mode === "toggle" ? "toggle" : "dial";
  const folder = a.settings?.folder || "";
  box.append(field("Mode", settingSelect(a, "mode", [["dial", "Cycle — turn to pick, push to toggle"], ["toggle", "One shader — press to toggle"]], () => renderInspector())));
  box.append(field("Shaders from", settingSelect(a, "folder", [["", "~/.config/hypr/shaders"], ["animated", "~/.config/hypr/shaders/animated"]], () => renderInspector())));
  if (mode === "toggle") {
    let shaders = [];
    try { shaders = await api("GET", `/api/shaders?folder=${encodeURIComponent(folder)}`); } catch {}
    box.append(field("Shader", settingSelect(a, "shader", [["", "Choose a shader…"], ...shaders.map((x) => [x.path, x.name])])));
  } else {
    box.append(el("p", { class: "muted" }, "Turning the dial picks a shader; pushing it (or tapping the strip) turns it on or off."));
  }
}

async function renderVolumeSettings(box, a) {
  if (!S.audio.length) {
    try { S.audio = await api("GET", "/api/audio"); } catch {}
  }
  const kind = a.settings?.kind === "source" ? "source" : "sink";
  const devices = S.audio.filter((d) => !d.monitor && (kind === "source" ? d.kind === "source" : d.kind === "sink"));
  const devSel = settingSelect(a, "device", [["", kind === "source" ? "Default input" : "Default output"], ...devices.map((d) => [d.name, d.description])]);
  const kindSel = settingSelect(a, "kind", [["sink", "Output (speakers, headset)"], ["source", "Input (microphone)"]], () => renderInspector());
  box.append(field("Type", kindSel), field("Device", devSel));
  box.append(el("div", { class: "row" },
    field("Name", settingInput(a, "name", { placeholder: "e.g. Speakers" })),
    field("Step (%)", settingInput(a, "step", { type: "number", placeholder: "2", min: 1, max: 25 }))));
  box.append(field("Icon", settingSelect(a, "icon", [["", "Automatic"], ["speaker", "Speaker"], ["headset", "Headset"], ["mic", "Microphone"]])));
  const mute = ["mute", "Toggle mute"], none = ["none", "Nothing"];
  // Blank means the daemon's default: push sets a chosen device as default, otherwise mutes.
  const pushOpts = a.settings?.device
    ? [["", "Set as default device"], mute, none]
    : [["", "Toggle mute"], none];
  box.append(el("div", { class: "row" },
    field("Push dial", settingSelect(a, "press", pushOpts)),
    field("Tap screen", settingSelect(a, "touch", [["", "Toggle mute"], ["default", "Set as default device"], none]))));
}

// ------------------------------------------------------------------ property inspectors

async function mountPI(holder, context, keep) {
  if (keep && S.pi?.context === context) {
    holder.append(keep);
    return;
  }
  unmountPI();
  holder.append(el("p", { class: "muted" }, "Loading settings…"));
  let info;
  try {
    info = await api("POST", `/api/devices/${S.serial}/inspector/${context}/open`);
  } catch (e) {
    holder.innerHTML = "";
    holder.append(el("p", { class: "muted" }, `Couldn't open settings: ${e.message}`));
    return;
  }
  if (!info.url) {
    holder.innerHTML = "";
    return;
  }
  const frame = el("iframe", { class: "pi-frame", src: info.url });
  frame.addEventListener("load", () => {
    try {
      const w = frame.contentWindow;
      const connect = w.connectElgatoStreamDeckSocket || w.connectSocket;
      if (typeof connect !== "function") throw new Error("page has no connectElgatoStreamDeckSocket");
      connect(info.port, info.uuid, info.registerEvent, JSON.stringify(info.info), JSON.stringify(info.actionInfo));
    } catch (e) {
      console.error("property inspector:", e);
    }
  });
  // Grow the frame with its content.
  const timer = setInterval(() => {
    try {
      const h = frame.contentDocument?.documentElement?.scrollHeight;
      if (h) frame.style.height = `${Math.min(h + 4, 1400)}px`;
    } catch {}
  }, 400);
  S.pi = { context, frame, timer };
  holder.innerHTML = "";
  holder.append(frame);
}

function unmountPI() {
  if (!S.pi) return;
  const { context, frame, timer } = S.pi;
  clearInterval(timer);
  frame.remove();
  S.pi = null;
  api("POST", `/api/devices/${S.serial}/inspector/${context}/close`).catch(() => {});
}

// ------------------------------------------------------------------ appearance

function displayState(slot) {
  const a = slot.actions[slot.display || 0] || slot.actions[0];
  a.states ||= [];
  const idx = a.state || 0;
  while (a.states.length <= idx) a.states.push({});
  return a.states[idx];
}

function renderAppearance(slot) {
  const box = el("div", { class: "section" }, el("div", { class: "section-head" }, el("h2", {}, "Appearance")));
  // Initial values come from st0; handlers write through curState() so a reloaded snapshot isn't missed.
  const curState = () => displayState(getSlot(S.sel) || slot);
  const st0 = curState();
  const changed = (soon = true) => (soon ? savePageSoon : savePage)();
  const refreshSoon = debounce(() => refreshSlot(S.sel), 450);
  const touch = (soon) => { changed(soon); refreshSoon(); };

  // Icon
  const current = st0.image;
  const thumbSrc = !current ? slotSrc(S.sel) : current.startsWith("glyph:") ? `/api/glyph?icon=${encodeURIComponent(current)}` : imgUrl(current);
  const upload = el("input", { type: "file", accept: "image/*", class: "hidden" });
  upload.addEventListener("change", async () => {
    const f = upload.files[0];
    if (!f) return;
    try {
      const res = await api("POST", `/api/icons?name=${encodeURIComponent(f.name)}`, f);
      curState().image = res.path;
      touch(false);
      renderInspector();
    } catch (e) { toast(e.message, true); }
  });
  box.append(field("Icon", el("div", { class: "icon-current" },
    el("div", { class: "thumb" }, el("img", { src: thumbSrc, alt: "" })),
    el("div", { class: "stack" },
      el("span", { class: "muted" }, !current ? "Default (from the action)" : current.startsWith("glyph:") ? current.slice(6).split("?")[0] : current.split("/").pop()),
      el("div", { class: "buttons" },
        el("button", { class: "btn small primary", onclick: () => openPicker((img) => { curState().image = img; touch(false); renderInspector(); }) }, "Choose icon…"),
        el("button", { class: "btn small", onclick: () => upload.click() }, "Upload…"),
        current ? el("button", { class: "btn small", onclick: () => { delete curState().image; touch(false); renderInspector(); } }, "Use default") : null)),
    upload)));

  // Background
  const bgOn = el("input", { type: "checkbox", checked: st0.background ? true : undefined });
  const bg = el("input", { type: "color", value: st0.background || "#000000" });
  const applyBg = () => {
    const st = curState();
    if (bgOn.checked) st.background = bg.value; else delete st.background;
    touch(true);
  };
  bgOn.addEventListener("change", applyBg);
  bg.addEventListener("input", () => { bgOn.checked = true; applyBg(); });
  box.append(field("Background", el("div", { class: "row" }, el("label", { class: "row", style: "flex:none" }, bgOn, "Colour"), bg)));

  // Labels
  const grid2 = el("div", { class: "labels-grid" });
  st0.labels ||= {};
  for (const pos of ["top", "center", "bottom"]) {
    const l = st0.labels[pos] || {};
    const text = el("input", { value: l.text || "", placeholder: "Text" });
    const size = el("input", { type: "number", value: l.size || "", placeholder: "16", min: 6, max: 60 });
    const color = el("input", { type: "color", value: l.color || "#ffffff" });
    const apply = () => {
      const st = curState();
      st.labels ||= {};
      if (!text.value) { delete st.labels[pos]; }
      else {
        st.labels[pos] = { text: text.value };
        if (size.value) st.labels[pos].size = Number(size.value);
        if (color.value && color.value !== "#ffffff") st.labels[pos].color = color.value;
      }
      touch(true);
    };
    for (const i of [text, size, color]) i.addEventListener("input", apply);
    grid2.append(el("span", {}, pos[0].toUpperCase() + pos.slice(1)), text, size, color);
  }
  box.append(field("Labels", grid2));
  box.append(el("p", { class: "muted" }, "Plugins may replace the icon or title while running (e.g. to show a live state)."));
  return box;
}

// ------------------------------------------------------------------ icon picker

const P = { onPick: null, source: "all", sets: [], packs: [], timer: null };

const pickerStyle = () => ({ color: $("#picker-color").value, scale: Number($("#picker-scale").value) / 100 });
const glyphRef = (icon) => {
  const { color, scale } = pickerStyle();
  return `glyph:${icon}?color=${encodeURIComponent(color)}&scale=${scale}`;
};

async function openPicker(onPick) {
  P.onPick = onPick;
  try {
    const saved = JSON.parse(localStorage.getItem("deckd.iconStyle") || "{}");
    if (saved.color) $("#picker-color").value = saved.color;
    if (saved.scale) $("#picker-scale").value = Math.round(saved.scale * 100);
  } catch {}
  applyPickerStyle();
  $("#picker").classList.remove("hidden");
  await loadIconSources();
  if (P.source === "all" && !P.sets.some((x) => x.installed)) P.source = "get";
  renderSources();
  runSearch();
  setTimeout(() => $("#picker-search").focus());
}

function closePicker() {
  $("#picker").classList.add("hidden");
  P.onPick = null;
}

async function loadIconSources() {
  const r = await api("GET", "/api/iconsets");
  P.sets = r.sets;
  P.packs = r.packs;
}

function renderSources() {
  const nav = $("#picker-sources");
  nav.innerHTML = "";
  const src = (id, label, extra) => el("button", {
    class: `src${P.source === id ? " active" : ""}`,
    onclick: () => { P.source = id; renderSources(); runSearch(); },
  }, el("span", {}, label), extra ? el("span", { class: "muted" }, extra) : null);
  nav.append(src("all", "All icon sets"), src("uploaded", "Uploaded"));
  const installed = P.sets.filter((x) => x.installed);
  if (installed.length) nav.append(el("h4", {}, "Icon sets"), ...installed.map((x) => src(x.prefix, x.name)));
  if (P.packs.length) nav.append(el("h4", {}, "Icon packs"), ...P.packs.map((x) => src(`pack:${x.id}`, x.name, x.count)));
  nav.append(el("h4", {}, "More"), src("get", "Get icon sets…"));
}

function applyPickerStyle() {
  const { color, scale } = pickerStyle();
  const res = $("#picker-results");
  res.style.color = color;
  res.style.setProperty("--glyph", `${Math.round(scale * 100)}%`);
  try { localStorage.setItem("deckd.iconStyle", JSON.stringify({ color, scale })); } catch {}
}

const runSearch = debounce(async () => {
  const res = $("#picker-results");
  const q = $("#picker-search").value.trim();
  res.innerHTML = "";
  $("#picker-note").textContent = "";
  if (P.source === "get") return renderGetSets(res);
  if (P.source === "uploaded") {
    let icons = await api("GET", "/api/icons");
    if (q) icons = icons.filter((i) => i.path.toLowerCase().includes(q.toLowerCase()));
    if (!icons.length) res.append(el("p", { class: "msg" }, "No uploaded icons yet. Use Upload… in the inspector."));
    for (const ic of icons) {
      res.append(el("div", { class: "ic", title: ic.path.split("/").pop(), onclick: () => pick(ic.path) }, el("img", { src: ic.url, alt: "" })));
    }
    return;
  }
  if (!q && !P.source.startsWith("pack:")) {
    res.append(el("p", { class: "msg" }, P.sets.some((x) => x.installed) ? "Type to search, e.g. “volume”, “microphone”, “obs”." : "No icon sets installed yet — pick one under “Get icon sets…”."));
    return;
  }
  const params = new URLSearchParams({ q });
  if (P.source !== "all") params.set("set", P.source);
  const results = await api("GET", `/api/iconsets/search?${params}`);
  if (!results.length) res.append(el("p", { class: "msg" }, "No icons match."));
  for (const r of results) {
    if (r.icon) {
      const tile = el("div", { class: "ic", title: r.icon, onclick: () => pick(glyphRef(r.icon)) }, el("span", { class: "nm" }, r.icon.split(":")[1]));
      tile.insertAdjacentHTML("afterbegin", r.svg);
      res.append(tile);
    } else {
      res.append(el("div", { class: "ic", title: r.name || "", onclick: () => pick(r.path) }, el("img", { src: r.url, alt: "" })));
    }
  }
  $("#picker-note").textContent = results.length >= 240 ? "Showing the first 240 matches — refine your search." : `${results.length} icons`;
}, 180);

function renderGetSets(res) {
  res.append(el("p", { class: "msg" }, "Icon sets are downloaded once from the jsDelivr CDN and then work offline. Search terms never leave your computer."));
  for (const set of P.sets) {
    const size = set.bytes ? `${(set.bytes / 1048576).toFixed(1)} MB` : "";
    const btn = el("button", { class: `btn small${set.installed ? " danger" : " primary"}` }, set.installed ? "Remove" : "Add");
    btn.addEventListener("click", async () => {
      btn.disabled = true;
      btn.textContent = set.installed ? "Removing…" : "Downloading…";
      try {
        if (set.installed) await api("DELETE", `/api/iconsets/${set.prefix}`);
        else await api("POST", `/api/iconsets/${set.prefix}`);
        await loadIconSources();
        renderSources();
        runSearch();
      } catch (e) { toast(e.message, true); btn.disabled = false; }
    });
    res.append(el("div", { class: "set-row" },
      el("div", {}, el("div", {}, set.name), el("div", { class: "muted" }, [set.prefix, set.license, size].filter(Boolean).join(" · "))),
      btn));
  }
  for (const pack of P.packs) {
    const btn = el("button", { class: "btn small danger" }, "Remove");
    btn.addEventListener("click", async () => {
      await api("DELETE", `/api/iconpacks/${encodeURIComponent(pack.id)}`).catch((e) => toast(e.message, true));
      await loadIconSources();
      renderSources();
      runSearch();
    });
    res.append(el("div", { class: "set-row" }, el("div", {}, el("div", {}, pack.name), el("div", { class: "muted" }, `Icon pack · ${pack.count} icons`)), btn));
  }
}

function pick(image) {
  const cb = P.onPick;
  closePicker();
  cb?.(image);
}

/** Re-applies the picker's colour and size to every library icon on the current page. */
async function restyleGlyphs() {
  const page = pageObj();
  let n = 0;
  for (const sect of ["keys", "dials"]) {
    for (const slot of Object.values(page[sect] || {})) {
      for (const a of slot.actions) {
        for (const st of a.states || []) {
          if (st.image?.startsWith("glyph:")) {
            st.image = glyphRef(st.image.slice(6).split("?")[0]);
            n++;
          }
        }
      }
    }
  }
  if (!n) return toast("No library icons on this page yet");
  await savePage();
  refreshAllSlots();
  toast(`Restyled ${n} icon${n === 1 ? "" : "s"}`);
  if (S.sel) renderInspector();
}

// ------------------------------------------------------------------ plugins tab

async function renderPlugins() {
  const list = $("#plugin-list");
  let plugins;
  try { plugins = await api("GET", "/api/plugins"); } catch (e) { return toast(e.message, true); }
  // Where each plugin is used on this device, so removal can warn.
  const used = {};
  for (const [pname, page] of Object.entries(S.snap?.pages || {})) {
    for (const slot of [...Object.values(page.keys || {}), ...Object.values(page.dials || {})]) {
      for (const a of slot.actions) {
        const owner = S.actions[a.uuid]?.plugin;
        if (owner) (used[owner] ||= new Set()).add(pname);
      }
    }
  }
  list.innerHTML = "";
  list.append(el("h2", { class: "list-head" }, `Installed plugins (${plugins.length})`));
  for (const p of plugins) {
    const pages = [...(used[p.uuid] || [])];
    const runtime = { node: "Node.js", native: "Native", html: "HTML", wine: "Windows (Wine)", none: "Can't run on Linux" }[p.runtime] || p.runtime;
    const remove = el("button", { class: "btn small danger" }, "Remove");
    remove.addEventListener("click", async () => {
      const warn = pages.length ? ` It's used on: ${pages.join(", ")}. Those buttons will stop working.` : "";
      if (!(await modal(`Remove ${p.name}?${warn}`, "", false))) return;
      try {
        await api("DELETE", `/api/plugins/${encodeURIComponent(p.uuid)}`);
        toast(`Removed ${p.name}`);
        await loadCatalog();
        renderPlugins();
        refreshAllSlots();
      } catch (e) { toast(e.message, true); }
    });
    list.append(el("div", { class: "plugin-item" },
      p.icon ? el("img", { src: p.icon, alt: "" }) : el("div", { class: "noicon" }),
      el("div", {},
        el("div", { class: "name" }, p.name),
        el("div", { class: "sub" }, `${p.author || "Unknown author"} · v${p.version} · ${p.actions.length} actions`),
        el("div", {},
          el("span", { class: `pill${p.runtime === "none" ? " bad" : ""}` }, runtime),
          p.running ? el("span", { class: "pill on" }, "running") : null,
          pages.length ? el("span", { class: "pill" }, `used on ${pages.length} page${pages.length > 1 ? "s" : ""}`) : null)),
      remove));
  }
}

async function installFromFile(file) {
  const btn = $("#plugin-install");
  btn.disabled = true;
  btn.textContent = `Installing ${file.name}…`;
  try {
    const r = await api("POST", `/api/plugins?name=${encodeURIComponent(file.name)}`, file);
    toast(r.iconPack ? `Installed icon pack ${r.iconPack}` : `Installed ${r.name} ${r.version}${r.runtime === "none" ? " — but it has no code that runs on Linux" : ""}`, r.runtime === "none");
    await loadCatalog();
    renderPlugins();
  } catch (e) {
    toast(`Install failed: ${e.message}`, true);
  } finally {
    btn.disabled = false;
    btn.textContent = "Install plugin or icon pack…";
  }
}

// ------------------------------------------------------------------ wiring

function renderAll() {
  renderPages();
  renderStageHead();
  renderPageSettings();
  renderDeck();
  renderInspector();
}

function init() {
  $("#device").addEventListener("change", (e) => {
    S.serial = e.target.value;
    S.dev = S.devices.find((d) => d.serial === S.serial);
    S.page = null;
    S.sel = null;
    unmountPI();
    loadSnapshot();
  });
  $("#page-add").addEventListener("click", addPage);
  $("#page-rename").addEventListener("click", () => renamePage());
  $("#page-delete").addEventListener("click", deletePage);
  $("#page-default").addEventListener("click", () => setProfile({ default_page: S.page }));
  $("#show-on-deck").addEventListener("click", () => api("POST", `/api/devices/${S.serial}/switch`, { page: S.page }));
  const bright = $("#brightness");
  bright.addEventListener("input", () => { $("#brightness-value").textContent = `${bright.value}%`; });
  bright.addEventListener("change", () => setProfile({ brightness: Number(bright.value) }));
  $("#search").addEventListener("input", renderLibrary);
  $("#plugin-install").addEventListener("click", () => $("#plugin-file").click());
  $("#plugin-file").addEventListener("change", (e) => { const f = e.target.files[0]; e.target.value = ""; if (f) installFromFile(f); });
  $("#picker-close").addEventListener("click", closePicker);
  $("#picker").addEventListener("click", (e) => { if (e.target.id === "picker") closePicker(); });
  $("#picker-search").addEventListener("input", runSearch);
  $("#picker-color").addEventListener("input", applyPickerStyle);
  $("#picker-scale").addEventListener("input", applyPickerStyle);
  $("#picker-restyle").addEventListener("click", restyleGlyphs);
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && !$("#picker").classList.contains("hidden")) closePicker(); });
  document.querySelectorAll(".tab").forEach((t) => t.addEventListener("click", () => showTab(t.dataset.tab)));
  document.addEventListener("keydown", (e) => {
    const typing = ["INPUT", "TEXTAREA", "SELECT"].includes(document.activeElement?.tagName);
    const modalOpen = !$("#picker").classList.contains("hidden") || !$("#modal").classList.contains("hidden");
    if ((e.key === "Delete" || e.key === "Backspace") && S.sel && !typing && !modalOpen) clearSlot();
  });
  window.addEventListener("resize", debounce(renderDeck, 150));
  connectEvents();
  loadCatalog().catch((e) => toast(e.message, true));
  loadDevices().catch((e) => toast(e.message, true));
}

init();
