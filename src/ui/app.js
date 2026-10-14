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
  try {
    const page = await api("PUT", `/api/devices/${S.serial}/pages/${encodeURIComponent(name)}`, pageObj());
    S.snap.pages[name] = page;
  } catch (e) {
    toast(`Couldn't save: ${e.message}`, true);
  }
}
const savePageSoon = debounce(savePage, 350);

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
  S.snap = await api("GET", `/api/devices/${S.serial}`);
  if (!S.snap.pages[S.page]) S.page = S.snap.current;
  renderPages();
  renderStageHead();
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
  const keyPx = window.innerWidth < 1100 ? 78 : 96, gap = 14;
  const keys = el("div", { class: "keys" });
  keys.style.gridTemplateColumns = `repeat(${S.dev.columns}, ${keyPx}px)`;
  for (let k = 0; k < S.dev.keys; k++) keys.append(makeSlot(`key-${k}`, "key"));
  deck.append(keys);

  if (S.dev.dials > 0) {
    const width = S.dev.columns * keyPx + (S.dev.columns - 1) * gap;
    if (S.dev.lcd) {
      const strip = el("div", { class: "strip" });
      strip.style.gridTemplateColumns = `repeat(${S.dev.dials}, 1fr)`;
      strip.style.width = `${width}px`;
      for (let d = 0; d < S.dev.dials; d++) strip.append(makeSlot(`dial-${d}`, "segment"));
      deck.append(strip);
    }
    const knobs = el("div", { class: "knobs" });
    knobs.style.gridTemplateColumns = `repeat(${S.dev.dials}, 1fr)`;
    knobs.style.width = `${width}px`;
    for (let d = 0; d < S.dev.dials; d++) {
      knobs.append(el("div", { class: `knob${S.sel === `dial-${d}` ? " selected" : ""}`, title: `Dial ${d + 1}`, onclick: () => select(`dial-${d}`) }));
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
        if (v) a.trigger = v; else delete a.trigger;
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
        onclick: (e) => { e.stopPropagation(); slot.display = i; savePage().then(() => { refreshSlot(S.sel); renderInspector(); }); },
      }, (slot.display || 0) === i ? "◉" : "○") : null,
      el("button", {
        class: "btn small", title: "Remove",
        onclick: async (e) => {
          e.stopPropagation();
          slot.actions.splice(i, 1);
          if ((slot.display || 0) >= slot.actions.length) slot.display = 0;
          setSlot(S.sel, slot.actions.length ? slot : null);
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
  const st = displayState(slot);
  const changed = (soon = true) => (soon ? savePageSoon : savePage)();
  const refreshSoon = debounce(() => refreshSlot(S.sel), 450);
  const touch = (soon) => { changed(soon); refreshSoon(); };

  // Icon
  const current = st.image;
  const preview = el("div", { class: "row" },
    el("span", { class: "muted", style: "flex:none" }, current ? "Custom icon" : "Default icon"),
    el("button", { class: "btn small", style: "flex:none", onclick: () => upload.click() }, "Upload…"),
    current ? el("button", { class: "btn small", style: "flex:none", onclick: () => { delete st.image; touch(false); renderInspector(); } }, "Use default") : null);
  const upload = el("input", { type: "file", accept: "image/*", class: "hidden" });
  upload.addEventListener("change", async () => {
    const f = upload.files[0];
    if (!f) return;
    try {
      const res = await api("POST", `/api/icons?name=${encodeURIComponent(f.name)}`, f);
      st.image = res.path;
      S.icons = [];
      touch(false);
      renderInspector();
    } catch (e) { toast(e.message, true); }
  });
  const grid = el("div", { class: "icon-grid" });
  box.append(field("Icon", el("div", { class: "stack" }, preview, upload, grid)));
  (async () => {
    if (!S.icons.length) {
      try { S.icons = await api("GET", "/api/icons"); } catch {}
    }
    for (const ic of S.icons) {
      grid.append(el("img", {
        src: ic.url, title: ic.path.split("/").pop(), class: ic.path === current ? "current" : "",
        onclick: () => { st.image = ic.path; touch(false); renderInspector(); },
      }));
    }
  })();

  // Background
  const bgOn = el("input", { type: "checkbox", checked: st.background ? true : undefined });
  const bg = el("input", { type: "color", value: st.background || "#000000" });
  const applyBg = () => { if (bgOn.checked) st.background = bg.value; else delete st.background; touch(true); };
  bgOn.addEventListener("change", applyBg);
  bg.addEventListener("input", () => { bgOn.checked = true; applyBg(); });
  box.append(field("Background", el("div", { class: "row" }, el("label", { class: "row", style: "flex:none" }, bgOn, "Colour"), bg)));

  // Labels
  const grid2 = el("div", { class: "labels-grid" });
  st.labels ||= {};
  for (const pos of ["top", "center", "bottom"]) {
    const l = st.labels[pos] || {};
    const text = el("input", { value: l.text || "", placeholder: "Text" });
    const size = el("input", { type: "number", value: l.size || "", placeholder: "16", min: 6, max: 60 });
    const color = el("input", { type: "color", value: l.color || "#ffffff" });
    const apply = () => {
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

// ------------------------------------------------------------------ wiring

function renderAll() {
  renderPages();
  renderStageHead();
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
  document.querySelectorAll(".tab").forEach((t) => t.addEventListener("click", () => showTab(t.dataset.tab)));
  document.addEventListener("keydown", (e) => {
    if ((e.key === "Delete" || e.key === "Backspace") && S.sel && !["INPUT", "TEXTAREA", "SELECT"].includes(document.activeElement?.tagName)) clearSlot();
  });
  window.addEventListener("resize", debounce(renderDeck, 200));
  connectEvents();
  loadCatalog().catch((e) => toast(e.message, true));
  loadDevices().catch((e) => toast(e.message, true));
}

init();
