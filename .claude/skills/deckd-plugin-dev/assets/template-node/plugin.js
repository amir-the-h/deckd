// __NAME__ — a dependency-free Stream Deck plugin (Node >= 22, uses the built-in WebSocket).
//
// deckd (and the official Stream Deck app) start this file with:
//   node plugin.js -port <port> -pluginUUID <uuid> -registerEvent <event> -info <json>
// Everything else happens over that one WebSocket. Protocol reference:
//   .claude/skills/deckd-plugin-dev/references/protocol.md

// --------------------------------------------------------------------------- startup

const args = {};
for (let i = 2; i + 1 < process.argv.length; i += 2) args[process.argv[i].replace(/^-/, "")] = process.argv[i + 1];
const info = JSON.parse(args.info || "{}");

const ws = new WebSocket(`ws://127.0.0.1:${args.port}`);
const send = (msg) => ws.readyState === WebSocket.OPEN && ws.send(JSON.stringify(msg));
const log = (message) => { console.log(message); send({ event: "logMessage", payload: { message } }); };

ws.onopen = () => {
  send({ event: args.registerEvent, uuid: args.pluginUUID });
  log(`__NAME__ registered (host ${info.application?.version} on ${info.application?.platform})`);
};
ws.onclose = () => process.exit(0); // the host went away; it will start us again when needed
ws.onmessage = (m) => {
  const msg = JSON.parse(m.data);
  try {
    handle(msg);
  } catch (e) {
    log(`error handling ${msg.event}: ${e.stack || e}`);
  }
};

// --------------------------------------------------------------------------- state

// Every on-screen action instance has a stable `context`; keep per-button state keyed by it.
// Settings arrive with every event and are the persisted source of truth.
const instances = new Map(); // context -> { action, controller, settings }

const DEFAULTS = { count: 0, step: 1, label: "" };
const settingsOf = (payload) => ({ ...DEFAULTS, ...(payload?.settings || {}) });

function remember(msg) {
  const inst = instances.get(msg.context) || {};
  inst.action = msg.action;
  inst.controller = msg.payload?.controller || inst.controller || "Keypad";
  inst.settings = settingsOf(msg.payload);
  instances.set(msg.context, inst);
  return inst;
}

function save(context, inst) {
  send({ event: "setSettings", context, payload: inst.settings }); // persisted by the host
  draw(context, inst);
}

// --------------------------------------------------------------------------- drawing

const keySvg = (text, color) =>
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 144 144"><rect width="144" height="144" fill="#1b1b1f"/>` +
  `<text x="72" y="92" font-family="sans-serif" font-size="${String(text).length > 3 ? 40 : 56}" font-weight="700" text-anchor="middle" fill="${color}">${text}</text></svg>`;

function draw(context, inst) {
  const { count, label } = inst.settings;
  if (inst.controller === "Encoder") {
    // Dials use the manifest's touch-strip layout ($A1: title, icon, value).
    send({ event: "setFeedback", context, payload: { title: label || "Counter", value: String(count) } });
  } else {
    const image = "data:image/svg+xml;base64," + Buffer.from(keySvg(count, "#f7821b")).toString("base64");
    send({ event: "setImage", context, payload: { image } });
    send({ event: "setTitle", context, payload: { title: label } });
  }
}

// --------------------------------------------------------------------------- events

function handle(msg) {
  switch (msg.event) {
    case "willAppear": {
      draw(msg.context, remember(msg));
      break;
    }
    case "willDisappear":
      instances.delete(msg.context);
      break;

    case "didReceiveSettings": // the settings panel (or the deckd UI) changed something
      draw(msg.context, remember(msg));
      break;

    case "keyUp": { // act on release, so tap/hold triggers configured in deckd behave naturally
      const inst = remember(msg);
      inst.settings.count += inst.settings.step;
      save(msg.context, inst);
      break;
    }

    case "dialRotate": {
      const inst = remember(msg);
      inst.settings.count += msg.payload.ticks * inst.settings.step;
      save(msg.context, inst);
      break;
    }
    case "dialDown":
    case "touchTap": {
      const inst = remember(msg);
      inst.settings.count = 0;
      save(msg.context, inst);
      send({ event: "showOk", context: msg.context });
      break;
    }

    case "sendToPlugin": // messages from pi/index.html
      if (msg.payload?.command === "reset") {
        const inst = instances.get(msg.context);
        if (inst) {
          inst.settings.count = 0;
          save(msg.context, inst);
        }
      }
      break;

    case "propertyInspectorDidAppear":
      send({ event: "sendToPropertyInspector", context: msg.context, payload: { hello: "from the plugin" } });
      break;
  }
}
