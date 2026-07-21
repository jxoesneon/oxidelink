// OxideLink frontend — WebSocket client bound to the Rust IPC server on :9001.
// Falls back to Tauri `invoke` for command-style calls when running inside Tauri.

import { escapeHtml, formatLogTimestamp, logLevelClass, buildBindingAction, parseBindingAction, RollingAverage, pushBuffer, buildLedMask, getCheckedLedIndices } from "./utils.js";

// Fallback WS URL used when Tauri invoke is unavailable (e.g. browser dev mode)
// or when get_ws_addr has not resolved yet. Must match `IPC_WS_ADDR` in
// `src-tauri/src/main.rs`.
const WS_URL_FALLBACK = "ws://127.0.0.1:9001";
const invoke = window.__TAURI__?.core?.invoke ?? null;

const el = (id) => document.getElementById(id);
const hidLog = el("hid-log");
const MAX_LOG_LINES = 40;

const appLogBody = el("app-log-body");
const logLevelFilter = el("log-level-filter");
const logSearch = el("log-search");
const logLiveToggle = el("log-live");
let appLogs = [];
let liveTail = true;
let pendingLogCount = 0;

// --- 1s rolling average with spike suppression for jittery telemetry ---
// RollingAverage class imported from ./utils.js
const signalAvg = new RollingAverage(1000);
// Battery is discrete (0/5/10/40/60/80/90/100%) — use a median filter to suppress
// brief level bounces. 2s window, no spike rejection (discrete values).
const batteryAvg = new RollingAverage(2000, Infinity);
const batteryEnhAvg = new RollingAverage(2000, Infinity);

function appendLog(text, cls = "hid-line") {
  const line = document.createElement("div");
  line.className = cls;
  line.textContent = text;
  hidLog.appendChild(line);
  while (hidLog.childElementCount > MAX_LOG_LINES) hidLog.firstChild.remove();
  hidLog.scrollTop = hidLog.scrollHeight;
}

function handleError(label, err) {
  console.error(label, err);
  appendLog(`[ERR] ${label}: ${err}`, "warn-line");
}

function flash(node) {
  node.classList.remove("flash");
  void node.offsetWidth;
  node.classList.add("flash");
}

// --- Diagnostics log viewer helpers ---
// escapeHtml, formatLogTimestamp, logLevelClass imported from ./utils.js

function appendLogBatch(logs) {
  if (!Array.isArray(logs)) return;
  appLogs.push(...logs);
  if (liveTail) {
    renderAppLogs();
  } else {
    pendingLogCount += logs.length;
    if (appLogBody) appLogBody.dataset.pending = pendingLogCount;
  }
  renderLoggingLogs();
}

function renderAppLogs() {
  if (!appLogBody) return;
  const level = (logLevelFilter?.value || "").toLowerCase();
  const query = (logSearch?.value || "").toLowerCase();
  const wrap = appLogBody.parentElement;

  const filtered = [];
  const maxRows = 500;
  for (let i = appLogs.length - 1; i >= 0; i--) {
    const e = appLogs[i];
    if (level && e.level !== level) continue;
    if (query && !(`${e.target || ""} ${e.message || ""}`.toLowerCase().includes(query))) continue;
    filtered.push(e);
    if (filtered.length >= maxRows) break;
  }
  filtered.reverse();

  const frag = document.createDocumentFragment();
  for (const e of filtered) {
    const tr = document.createElement("tr");
    tr.className = logLevelClass(e.level);
    tr.innerHTML = `<td>${escapeHtml(formatLogTimestamp(e.timestamp))}</td><td>${escapeHtml(e.level)}</td><td>${escapeHtml(e.target)}</td><td>${escapeHtml(e.message)}</td>`;
    frag.appendChild(tr);
  }
  appLogBody.innerHTML = "";
  appLogBody.appendChild(frag);
  pendingLogCount = 0;
  if (appLogBody.dataset) appLogBody.dataset.pending = "0";

  if (liveTail && wrap) {
    wrap.scrollTop = wrap.scrollHeight;
  }
}

async function refreshAppLogs() {
  if (!invoke) return;
  try {
    const level = logLevelFilter?.value || null;
    const search = logSearch?.value || null;
    const logs = await invoke("get_logs", { level, search, limit: 1000 });
    appLogs = logs || [];
    renderAppLogs();
  } catch (e) {
    appendLog("[ERR] get_logs failed: " + e, "warn-line");
  }
}

async function copyAppLogs() {
  const text = appLogs
    .map((e) => `${formatLogTimestamp(e.timestamp)} [${e.level}] ${e.target}: ${e.message}`)
    .join("\n");
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      const ta = document.createElement("textarea");
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      document.body.removeChild(ta);
    }
  } catch (e) {
    appendLog("[ERR] copy logs failed: " + e, "warn-line");
  }
}

async function clearAppLogs() {
  if (!invoke) return;
  try {
    await invoke("clear_logs");
    appLogs = [];
    renderAppLogs();
  } catch (e) {
    appendLog("[ERR] clear_logs failed: " + e, "warn-line");
  }
}

// Track the current connection transport ("USB" / "Bluetooth") so the
// status chip can show it alongside the connected/disconnected state.
let currentTransport = null;

function setConnection(state) {
  const chip = el("connection-chip");
  const dot = el("conn-dot");
  const label = el("conn-label");
  chip.classList.remove("connected", "disconnected");
  const suffix = currentTransport ? ` · ${currentTransport}` : "";
  if (state === "connected") {
    chip.classList.add("connected");
    label.textContent = `Connected${suffix}`;
  } else if (state === "disconnected") {
    chip.classList.add("disconnected");
    label.textContent = "Disconnected";
    currentTransport = null;
  } else {
    label.textContent = "Connecting…";
  }
  flash(chip);
  updateBtSwitchButton();
}

// Show the "Switch to BT" button only when the controller is connected
// over USB. Hidden when disconnected, connecting, or already on Bluetooth.
function updateBtSwitchButton() {
  const btn = el("btn-bt-reconnect");
  if (!btn) return;
  const chip = el("connection-chip");
  const isConnected = chip && chip.classList.contains("connected");
  const isUsb = currentTransport && currentTransport.toLowerCase().includes("usb");
  btn.hidden = !(isConnected && isUsb);
}

function updateBattery(percent, charging) {
  const fill = el("battery-fill");
  const val = el("battery-value");
  // Median filter — battery is discrete, median is more stable than avg
  batteryAvg.push(percent);
  const med = batteryAvg.median();
  const pct = Math.max(0, Math.min(100, med !== null ? med : percent));
  fill.style.width = pct + "%";
  fill.classList.toggle("low", pct <= 15);
  val.textContent = Math.round(pct) + "%";
  val.classList.toggle("warn", pct <= 15);
  el("charging-value").textContent = charging ? "Yes" : "No";
  el("charging-value").classList.toggle("ok", charging);
  flash(el("panel-telemetry"));
}

function updateKeepAlive(data) {
  el("ka-status").textContent = data.active ? "Active" : "Idle";
  el("ka-status").classList.toggle("ok", data.active);
  el("ka-interval").textContent = data.interval_ms + " ms";
  el("ka-power-events").textContent = data.power_events_detected;
  if (data.adaptive_mode) el("ka-interval").classList.add("warn");
  else el("ka-interval").classList.remove("warn");
}

function updateConfig(cfg) {
  el("dz-left").value = Math.round(cfg.deadzone_left * 100);
  el("dz-right").value = Math.round(cfg.deadzone_right * 100);
  el("dz-left-val").textContent = cfg.deadzone_left.toFixed(2);
  el("dz-right-val").textContent = cfg.deadzone_right.toFixed(2);
  el("remap-a").value = cfg.button_remap.a_to;
  el("remap-b").value = cfg.button_remap.b_to;
  el("remap-x").value = cfg.button_remap.x_to;
  el("remap-y").value = cfg.button_remap.y_to;
  el("mock-badge").style.display = cfg.mock_mode ? "" : "none";

  const vc = cfg.default_virtual_controller || "xbox360";
  const vcXbox = el("vc-xbox360");
  const vcDs4 = el("vc-dualshock4");
  if (vcXbox) vcXbox.checked = vc === "xbox360";
  if (vcDs4) vcDs4.checked = vc === "dualshock4";

  // Sync mapping panel
  if (cfg.mappings) {
    if (el("binding-global-interval")) {
      el("binding-global-interval").value = cfg.mappings.turbo_interval_ms ?? 100;
    }
    if (el("binding-global-duty")) {
      el("binding-global-duty").value = cfg.mappings.turbo_duty_cycle ?? 0.5;
    }
    syncBindingRows(cfg.mappings.buttons || []);
  }
}

// Populate remap selects
const BUTTONS = ["a", "b", "x", "y", "l", "r", "zl", "zr", "minus", "plus", "home", "capture"];
["remap-a", "remap-b", "remap-x", "remap-y"].forEach((sid) => {
  const sel = el(sid);
  BUTTONS.forEach((b) => {
    const o = document.createElement("option");
    o.value = b;
    o.textContent = b.toUpperCase();
    sel.appendChild(o);
  });
});

// --- Button turbo / toggle bindings ---
const BINDING_BUTTONS = [
  { id: "a", label: "A" },
  { id: "b", label: "B" },
  { id: "x", label: "X" },
  { id: "y", label: "Y" },
  { id: "up", label: "D-Up" },
  { id: "down", label: "D-Down" },
  { id: "left", label: "D-Left" },
  { id: "right", label: "D-Right" },
  { id: "l", label: "L" },
  { id: "r", label: "R" },
  { id: "zl", label: "ZL" },
  { id: "zr", label: "ZR" },
  { id: "minus", label: "-" },
  { id: "plus", label: "+" },
  { id: "home", label: "Home" },
  { id: "capture", label: "Capture" },
  { id: "lstick", label: "L Stick" },
  { id: "rstick", label: "R Stick" },
];

// buildBindingAction and parseBindingAction imported from ./utils.js

function ensureMappings() {
  if (!currentConfig.mappings) {
    currentConfig.mappings = { buttons: [], turbo_interval_ms: 100, turbo_duty_cycle: 0.5 };
  }
  if (!Array.isArray(currentConfig.mappings.buttons)) {
    currentConfig.mappings.buttons = [];
  }
}

function renderBindingsList() {
  const container = el("bindings-list");
  if (!container) return;
  container.innerHTML = "";
  BINDING_BUTTONS.forEach((btn) => {
    const row = document.createElement("div");
    row.className = "binding-row";
    row.dataset.source = btn.id;

    const label = document.createElement("label");
    label.textContent = btn.label;
    row.appendChild(label);

    const target = document.createElement("select");
    target.className = "binding-target";
    BINDING_BUTTONS.forEach((t) => {
      const o = document.createElement("option");
      o.value = t.id;
      o.textContent = t.label;
      target.appendChild(o);
    });

    const mode = document.createElement("select");
    mode.className = "binding-mode";
    [
      { value: "normal", text: "Normal" },
      { value: "turbo", text: "Turbo" },
      { value: "toggle", text: "Toggle" },
    ].forEach((m) => {
      const o = document.createElement("option");
      o.value = m.value;
      o.textContent = m.text;
      mode.appendChild(o);
    });

    const interval = document.createElement("input");
    interval.type = "number";
    interval.className = "binding-interval";
    interval.min = 10;
    interval.max = 5000;
    interval.step = 10;
    interval.value = 100;
    interval.title = "Turbo interval (ms)";

    const status = document.createElement("span");
    status.className = "binding-status";
    status.textContent = "";

    function onChange() {
      const src = row.dataset.source;
      const tgt = target.value;
      const md = mode.value;
      const iv = parseInt(interval.value, 10) || 100;
      interval.disabled = md !== "turbo";
      status.textContent = md === "normal" ? "" : `${md} \u2192 ${tgt.toUpperCase()}`;

      ensureMappings();
      const action = buildBindingAction(md, tgt, iv);
      const existing = currentConfig.mappings.buttons.find((m) => m.source === src);
      if (existing) {
        existing.actions = [action];
      } else {
        currentConfig.mappings.buttons.push({ source: src, actions: [action] });
      }
      pushConfig(currentConfig);
    }

    target.addEventListener("change", onChange);
    mode.addEventListener("change", onChange);
    interval.addEventListener("change", onChange);

    row.appendChild(target);
    row.appendChild(mode);
    row.appendChild(interval);
    row.appendChild(status);
    container.appendChild(row);
  });
}

function syncBindingRows(buttons) {
  const rows = document.querySelectorAll(".binding-row");
  rows.forEach((row) => {
    const src = row.dataset.source;
    const mapping = (buttons || []).find((m) => m.source === src);
    const target = row.querySelector(".binding-target");
    const mode = row.querySelector(".binding-mode");
    const interval = row.querySelector(".binding-interval");
    const status = row.querySelector(".binding-status");
    const parsed = parseBindingAction(mapping?.actions?.[0]);
    target.value = parsed.target;
    mode.value = parsed.mode;
    interval.value = parsed.interval;
    interval.disabled = parsed.mode !== "turbo";
    status.textContent = parsed.mode === "normal" ? "" : `${parsed.mode} \u2192 ${parsed.target.toUpperCase()}`;
  });
}

function bindGlobalTurboInputs() {
  const intervalEl = el("binding-global-interval");
  const dutyEl = el("binding-global-duty");
  if (!intervalEl || !dutyEl) return;
  function pushGlobal() {
    ensureMappings();
    currentConfig.mappings.turbo_interval_ms = parseInt(intervalEl.value, 10) || 100;
    currentConfig.mappings.turbo_duty_cycle = parseFloat(dutyEl.value) || 0.5;
    pushConfig(currentConfig);
  }
  intervalEl.addEventListener("change", pushGlobal);
  dutyEl.addEventListener("change", pushGlobal);
}

renderBindingsList();
bindGlobalTurboInputs();

// --- Controller wireframe ---
const STICK_RADIUS = 12; // max pixel offset of cap inside its housing

function toggleBtn(id, active) {
  const node = el(id);
  if (!node) return;
  node.classList.toggle("active", active);
}

function updateStick(capId, stick) {
  const cap = el(capId);
  if (!cap) return;
  // stick.x / stick.y are -1..1; +y is up, so invert for SVG (y grows down).
  const dx = Math.max(-1, Math.min(1, stick.x)) * STICK_RADIUS;
  const dy = -Math.max(-1, Math.min(1, stick.y)) * STICK_RADIUS;
  cap.setAttribute("transform", `translate(${dx} ${dy})`);
}

function updateWireframe(data) {
  const b = data.buttons || {};
  toggleBtn("btn-a", b.a);
  toggleBtn("btn-b", b.b);
  toggleBtn("btn-x", b.x);
  toggleBtn("btn-y", b.y);
  toggleBtn("btn-l", b.l);
  toggleBtn("btn-r", b.r);
  toggleBtn("btn-zl", b.zl);
  toggleBtn("btn-zr", b.zr);
  toggleBtn("btn-minus", b.minus);
  toggleBtn("btn-plus", b.plus);
  toggleBtn("btn-home", b.home);
  toggleBtn("btn-capture", b.capture);
  toggleBtn("stick-left-cap", b.stick_l);
  toggleBtn("stick-right-cap", b.stick_r);
  toggleBtn("dpad-up", b.dpad_up);
  toggleBtn("dpad-down", b.dpad_down);
  toggleBtn("dpad-left", b.dpad_left);
  toggleBtn("dpad-right", b.dpad_right);

  if (data.left_stick) {
    updateStick("stick-left-cap", data.left_stick);
    updateStickCalDotFromState("left", data.left_stick);
  }
  if (data.right_stick) {
    updateStick("stick-right-cap", data.right_stick);
    updateStickCalDotFromState("right", data.right_stick);
  }
}

// High-frequency event batching: buffer the latest ControllerState, ImuData,
// and ConnectionQuality events and process them in a requestAnimationFrame
// loop. This prevents DOM backpressure from stalling the WebSocket at
// ~150 events/sec. Low-frequency events are handled immediately.
let _pendingState = null;
let _pendingImu = null;
let _pendingConnQuality = null;
let _rafScheduled = false;

function _flushPendingHighFreq() {
  _rafScheduled = false;
  try {
    if (_pendingState) {
      const ev = _pendingState;
      _pendingState = null;
      // Extract transport from the ControllerState event — this fires at
      // ~72Hz so the transport label stays in sync without waiting for the
      // slower DeviceInfo event.
      const ct = ev.data.connection_type;
      if (ct) {
        const newTransport = (typeof ct === "string" ? ct : String(ct)).replace(/^.*::/, "");
        const norm = newTransport.toLowerCase().includes("usb") ? "USB"
                   : newTransport.toLowerCase().includes("blue") ? "Bluetooth"
                   : newTransport;
        if (norm !== currentTransport) {
          currentTransport = norm;
          updateBtSwitchButton();
        }
      }
      setConnection(ev.data.connected ? "connected" : "disconnected");
      updateBattery(ev.data.battery_percent, ev.data.charging);
      signalAvg.push(ev.data.signal_strength);
      const sigAvg = signalAvg.avg();
      el("signal-value").textContent = (sigAvg !== null ? sigAvg.toFixed(1) : ev.data.signal_strength) + " dBm";
      updateWireframe(ev.data);
      if (ev.data.battery_voltage_mv && ev.data.battery_voltage_mv > 0) {
        const bvEl = el("battery-voltage");
        if (bvEl) bvEl.textContent = ev.data.battery_voltage_mv + " mV";
      }
      if (ev.data.nfc && ev.data.nfc.scan_count !== undefined) {
        setText("nfc-scan-count", String(ev.data.nfc.scan_count));
      }
    }
    if (_pendingImu) {
      const ev = _pendingImu;
      _pendingImu = null;
      updateImuDisplay(ev);
    }
    if (_pendingConnQuality) {
      const ev = _pendingConnQuality;
      _pendingConnQuality = null;
      updateConnectionQuality(ev.data);
    }
  } catch (e) {
    appendLog("[ERR] _flushPendingHighFreq: " + e + " — " + (e.stack || "").split("\n")[1], "warn-line");
  }
}

function _scheduleFlush() {
  if (!_rafScheduled) {
    _rafScheduled = true;
    requestAnimationFrame(_flushPendingHighFreq);
  }
}

function handleEvent(ev) {
  switch (ev.type) {
    case "ControllerState":
      _pendingState = ev;
      _scheduleFlush();
      break;
    case "ImuData":
      _pendingImu = ev;
      _scheduleFlush();
      break;
    case "ConnectionQuality":
      _pendingConnQuality = ev;
      _scheduleFlush();
      break;
    case "KeepAliveStatus":
      updateKeepAlive(ev.data);
      break;
    case "ConfigUpdated":
      updateConfig(ev.data);
      break;
    case "BatteryWarning":
      appendLog(`[WARN] Battery low: ${ev.percent}%`, "warn-line");
      break;
    case "Disconnected":
      setConnection("disconnected");
      appendLog(`[WARN] Disconnected: ${ev.reason}`, "warn-line");
      break;
    case "Reconnected":
      setConnection("connected");
      appendLog("[OK] Reconnected", "hid-line");
      break;
    case "BluetoothPowerEvent":
      appendLog(`[WARN] BT power event: ${ev.event_type} @ ${ev.timestamp}`, "warn-line");
      break;
    case "RawHidReport":
      appendLog(`HID 0x${ev.report_id.toString(16).padStart(2, "0")}: ${ev.hex}`);
      break;
    case "LogMessage":
      appendLog(`[${ev.level}] ${ev.message}`, ev.level === "warn" ? "warn-line" : "hid-line");
      break;
    case "LogBatch":
      if (ev.logs) appendLogBatch(ev.logs);
      break;
    case "DeviceInfo":
      updateDeviceInfo(ev.data);
      break;
    case "CalibrationData":
      updateStickCalibration(ev);
      break;
    case "CalibrationStatus":
      updateCalibrationStatus(ev.data || ev);
      break;
    case "PlayerLightsChanged":
      syncPlayerLeds(ev);
      break;
    case "HomeLightChanged":
      syncHomeLight(ev);
      break;
    case "SubcommandReply":
      appendLog(`[SUBCMD] 0x${(ev.subcmd_id || 0).toString(16).padStart(2, "0")}: ack=0x${(ev.ack || 0).toString(16).padStart(2, "0")}`);
      break;
    case "BatteryState":
      updateBatteryEnhanced(ev);
      break;
    case "NfcTagScanned":
      updateNfcTagDisplay(ev.tag);
      break;
    case "NfcModeChanged":
      if (el("nfc-mode-select")) el("nfc-mode-select").value = String(ev.mode === undefined ? 0 : (ev.mode.Disabled ? 0 : ev.mode.Nfc ? 1 : ev.mode.IrCamera ? 2 : ev.mode.Passthrough ? 3 : 0));
      setText("nfc-status-text", ev.mode ? "Active" : "Inactive");
      break;
    case "IrFrameReceived":
      // IR frame received — update NFC/IR status
      setText("nfc-status-text", "IR Frame");
      if (ev.frame) {
        setText("nfc-tag-uid", "IR " + (ev.frame.width || 0) + "x" + (ev.frame.height || 0));
        setText("nfc-tag-type", "IR Camera");
        setText("nfc-tag-amiibo", "No");
        setText("nfc-tag-size", (ev.frame.frame_data?.length || 0) + " bytes");
        if (el("nfc-tag-info")) el("nfc-tag-info").style.display = "block";
      }
      break;
    case "ProfileChanged":
      updateProfileIndicator(ev.profile_id, ev.profile_name);
      break;
    case "TrayStateChanged":
      updateTrayStateDisplay(ev.data);
      break;
  }
}

function updateTrayStateDisplay(state) {
  const visible = state?.visible ?? true;
  const minimized = state?.minimized ?? false;
  const autoStart = state?.auto_start ?? false;
  const text = `${visible ? "Visible" : "Hidden"} · ${minimized ? "Minimized to tray" : "Not minimized"} · Auto-start ${autoStart ? "on" : "off"}`;
  const node = el("tray-state-display");
  if (node) node.textContent = text;
}

async function refreshTrayState() {
  if (!invoke) return;
  try {
    const state = await invoke("get_tray_state");
    updateTrayStateDisplay(state);
  } catch (err) {
    appendLog("[ERR] get_tray_state failed: " + err, "warn-line");
  }
}

function connect() {
  setConnection("connecting");
  // Resolve the WS address from the backend when running inside Tauri so the
  // frontend doesn't need to hardcode the port. Fall back to the compile-time
  // default when invoke is unavailable (e.g. browser dev mode).
  const addrPromise = invoke
    ? invoke("get_ws_addr").then((a) => "ws://" + a).catch(() => WS_URL_FALLBACK)
    : Promise.resolve(WS_URL_FALLBACK);
  addrPromise.then((wsUrl) => {
    let ws;
    try {
      ws = new WebSocket(wsUrl);
    } catch (e) {
      appendLog("[ERR] WebSocket unavailable: " + e, "warn-line");
      setTimeout(connect, 2000);
      return;
    }
    ws.onopen = () => appendLog("[OK] IPC connected to " + wsUrl);
    ws.onmessage = (msg) => {
      try {
        handleEvent(JSON.parse(msg.data));
      } catch (e) {
        appendLog("[ERR] Bad IPC payload: " + e, "warn-line");
      }
    };
    ws.onclose = () => {
      setConnection("disconnected");
      appendLog("[WARN] IPC closed — retrying in 2s", "warn-line");
      setTimeout(connect, 2000);
    };
    ws.onerror = () => {
      appendLog("[ERR] IPC socket error", "warn-line");
    };
  });
}

// Config controls — push updates via Tauri invoke (if available) or just local.
async function pushConfig(cfg) {
  if (invoke) {
    try {
      await invoke("update_config", { config: cfg });
    } catch (e) {
      appendLog("[ERR] update_config failed: " + e, "warn-line");
    }
  }
  updateConfig(cfg);
}

let currentConfig = {
  deadzone_left: 0.08,
  deadzone_right: 0.08,
  keepalive_interval_ms: 3000,
  adaptive_keepalive: true,
  battery_warning_threshold: 15,
  button_remap: { a_to: "b", b_to: "a", x_to: "y", y_to: "x" },
  mock_mode: false,
  config_persistence_enabled: true,
  auto_reconnect: true,
  reconnect_interval_s: 3,
  bt_power_detection_enabled: true,
  battery_polling_interval_s: 30,
  close_to_tray: true,
  tray_minimize: true,
  auto_start: false,
  default_virtual_controller: "xbox360",
  hidhide_enabled: false,
  hidhide_auto_hide: false,
  mappings: {
    buttons: [],
    turbo_interval_ms: 100,
    turbo_duty_cycle: 0.5,
  },
  notification_config: {
    enabled: true,
    critical_enabled: true,
    warning_enabled: true,
    info_enabled: true,
    notify_disconnect: true,
    notify_bt_power: true,
    notify_low_battery: true,
    notify_drift: true,
    notify_reconnect: true,
  },
};

function readConfigFromControls() {
  currentConfig.deadzone_left = parseInt(el("dz-left").value, 10) / 100;
  currentConfig.deadzone_right = parseInt(el("dz-right").value, 10) / 100;
  currentConfig.button_remap.a_to = el("remap-a").value;
  currentConfig.button_remap.b_to = el("remap-b").value;
  currentConfig.button_remap.x_to = el("remap-x").value;
  currentConfig.button_remap.y_to = el("remap-y").value;
  return currentConfig;
}

["dz-left", "dz-right"].forEach((id) =>
  el(id).addEventListener("input", () => {
    el(id + "-val").textContent = parseInt(el(id).value, 10) / 100;
  })
);
["dz-left", "dz-right", "remap-a", "remap-b", "remap-x", "remap-y"].forEach((id) =>
  el(id).addEventListener("change", () => pushConfig(readConfigFromControls()))
);

el("btn-boost").addEventListener("click", async () => {
  if (invoke) {
    try {
      await invoke("trigger_keepalive_boost");
      appendLog("[OK] Keep-alive boost triggered", "hid-line");
    } catch (e) {
      appendLog("[ERR] boost failed: " + e, "warn-line");
    }
  } else {
    appendLog("[INFO] Boost requires Tauri invoke (not in browser)", "warn-line");
  }
});

// Boot
appendLog("OxideLink frontend initialized");
updateConfig(currentConfig);
connect();

// Load the persisted config (including mappings) from the backend when in Tauri.
if (invoke) {
  (async () => {
    try {
      const cfg = await invoke("get_config");
      if (cfg) {
        currentConfig = { ...currentConfig, ...cfg };
        updateConfig(currentConfig);
      }
    } catch (e) {
      /* ignore — backend may not be ready yet; ConfigUpdated will follow */
    }
  })();
}

// Poll XInput hex via Tauri invoke every second (when in Tauri).
if (invoke) {
  setInterval(async () => {
    try {
      const hex = await invoke("get_xinput_hex");
      el("xinput-hex").textContent = "XInput: " + hex;
    } catch (e) {
      /* ignore */
    }
  }, 1000);

  // One-shot controller state poll on startup (the ControllerState WS event
  // may fire before the WebSocket connects). Do NOT poll continuously — it
  // causes the connection chip to flash repeatedly (pulsating effect).
  setTimeout(async () => {
    try {
      const state = await invoke("get_controller_state");
      if (state.connected) {
        setConnection("connected");
        updateBattery(state.battery_percent, state.charging);
      }
    } catch (e) {
      /* ignore — backend may not be ready yet */
    }
  }, 1500);

  // Fetch calibration data on load (the CalibrationData event may have fired
  // before the WebSocket connected, so we poll it once). The CalibrationData
  // event includes both stick and IMU calibration, so we fetch both and
  // combine them into the expected event shape.
  setTimeout(async () => {
    try {
      const [stick, imu] = await Promise.all([
        invoke("get_calibration_data"),
        invoke("get_imu_calibration"),
      ]);
      if (stick || imu) updateStickCalibration({ stick, imu });
    } catch (e) {
      /* ignore — not connected yet */
    }
  }, 2000);

  // Fetch device info on load (the DeviceInfo event may have fired before
  // the WebSocket connected, so we poll it once). This populates the Device
  // Info and SPI Flash panels in the Diagnostics tab.
  setTimeout(async () => {
    try {
      const info = await invoke("get_device_info");
      if (info) updateDeviceInfo(info);
    } catch (e) {
      /* ignore — not connected yet */
    }
  }, 2500);

  // Fetch keepalive status on load (the KeepAliveStatus event fires every 3s,
  // but the first one may not have arrived yet). This populates the keepalive
  // status chip immediately instead of showing "Idle / — ms / 0".
  setTimeout(async () => {
    try {
      const ka = await invoke("get_keepalive_status");
      if (ka) updateKeepAlive(ka);
    } catch (e) {
      /* ignore — backend may not be ready yet */
    }
  }, 1500);

  // Fetch home light state on load so the UI reflects the actual controller
  // state instead of defaults. The HomeLightChanged event only fires when the
  // user changes the home light, so without this poll the UI shows defaults.
  setTimeout(async () => {
    try {
      const hl = await invoke("get_home_light");
      if (hl) {
        const patternNames = ["solid", "breathing", "blink", "fade", "wave"];
        syncHomeLight({
          enabled: hl.enabled,
          brightness: hl.brightness,
          pattern: patternNames[hl.pulse_pattern] || "solid",
        });
      }
    } catch (e) {
      /* ignore — not connected yet */
    }
  }, 2500);

  // Fetch IMU sensitivity on load so the dropdowns reflect the actual
  // controller state instead of defaults. There is no IPC event for IMU
  // sensitivity changes, so without this poll the UI shows defaults.
  setTimeout(async () => {
    try {
      const [gyroRange, accelRange] = await invoke("get_imu_sensitivity");
      const gyroSel = el("imu-gyro-range");
      const accelSel = el("imu-accel-range");
      if (gyroSel && gyroRange != null) gyroSel.value = String(gyroRange);
      if (accelSel && accelRange != null) accelSel.value = String(accelRange);
    } catch (e) {
      /* ignore — not connected yet */
    }
  }, 2500);

  // Fetch player lights on load so the LED indicators reflect the actual
  // controller state. The PlayerLightsChanged event only fires when the user
  // changes the player lights, so without this poll the UI shows defaults.
  setTimeout(async () => {
    try {
      const lights = await invoke("get_player_lights");
      if (lights) {
        // Update LED toggles to match the controller's current state.
        for (let i = 0; i < 4; i++) {
          const toggle = el(`led-toggle-${i + 1}`);
          if (toggle) toggle.checked = (lights.led_mask & (1 << i)) !== 0;
        }
      }
    } catch (e) {
      /* ignore — not connected yet */
    }
  }, 2500);
}

// =============================================================================
// Tab navigation
// =============================================================================
document.querySelectorAll(".tab-btn").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab-btn").forEach((b) => b.classList.remove("active"));
    document.querySelectorAll(".tab-pane").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    const pane = el(`tab-${btn.dataset.tab}`);
    if (pane) pane.classList.add("active");
  });
});

// =============================================================================
// Diagnostics log viewer controls
// =============================================================================
if (logLevelFilter) logLevelFilter.addEventListener("change", renderAppLogs);
if (logSearch) logSearch.addEventListener("input", renderAppLogs);
if (logLiveToggle) {
  liveTail = logLiveToggle.checked;
  logLiveToggle.addEventListener("change", () => {
    liveTail = logLiveToggle.checked;
    if (liveTail) renderAppLogs();
  });
}
if (el("btn-copy-logs")) el("btn-copy-logs").addEventListener("click", copyAppLogs);
if (el("btn-clear-logs")) el("btn-clear-logs").addEventListener("click", clearAppLogs);

document.querySelectorAll(".tab-btn").forEach((btn) => {
  if (btn.dataset.tab === "diagnostics") {
    btn.addEventListener("click", () => refreshAppLogs());
  }
});

// =============================================================================
// IMU — sparklines, attitude indicator, controls
// =============================================================================
const ACCEL_SCALE = 1 / 4096; // g per LSB
const GYRO_SCALE = 1 / 13371; // deg/s per LSB
const IMU_BUFFER_SIZE = 100;
const imuBuffers = {
  accelX: [],
  accelY: [],
  accelZ: [],
  gyroX: [],
  gyroY: [],
  gyroZ: [],
};
let imuEnabled = false;
let gyroAimEnabled = false;
let gyroSensitivity = 0.05;

function drawSparkline(canvasId, buffer, color) {
  const canvas = el(canvasId);
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  if (buffer.length < 2) return;
  let min = Infinity;
  let max = -Infinity;
  for (const v of buffer) {
    if (v < min) min = v;
    if (v > max) max = v;
  }
  if (max - min < 1e-9) {
    max = min + 1;
  }
  const pad = 2;
  const range = max - min;
  ctx.strokeStyle = color;
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  for (let i = 0; i < buffer.length; i++) {
    const x = (i / (IMU_BUFFER_SIZE - 1)) * (w - pad * 2) + pad;
    const y = h - pad - ((buffer[i] - min) / range) * (h - pad * 2);
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

// pushBuffer imported from ./utils.js

function updateImuDisplay(imuData) {
  // The backend sends IpcEvent::ImuData { frames: ImuData { frames: [ImuFrame; 3] }, timestamp }
  // So ev.frames is an object with a .frames array, OR (legacy) an array directly.
  const rawFrames = imuData.frames;
  const frames = Array.isArray(rawFrames) ? rawFrames : (rawFrames?.frames || rawFrames?.frame || []);
  if (!frames.length) return;
  const f = frames[0];
  // Raw LSB values
  const ax = f.accel_x ?? f.accelX ?? 0;
  const ay = f.accel_y ?? f.accelY ?? 0;
  const az = f.accel_z ?? f.accelZ ?? 0;
  const gx = f.gyro_x ?? f.gyroX ?? 0;
  const gy = f.gyro_y ?? f.gyroY ?? 0;
  const gz = f.gyro_z ?? f.gyroZ ?? 0;
  // Convert to physical units
  const axG = ax * ACCEL_SCALE;
  const ayG = ay * ACCEL_SCALE;
  const azG = az * ACCEL_SCALE;
  const gxDS = gx * GYRO_SCALE;
  const gyDS = gy * GYRO_SCALE;
  const gzDS = gz * GYRO_SCALE;
  // Update buffers
  pushBuffer(imuBuffers.accelX, axG);
  pushBuffer(imuBuffers.accelY, ayG);
  pushBuffer(imuBuffers.accelZ, azG);
  pushBuffer(imuBuffers.gyroX, gxDS);
  pushBuffer(imuBuffers.gyroY, gyDS);
  pushBuffer(imuBuffers.gyroZ, gzDS);
  // Draw sparklines
  drawSparkline("spark-accel-x", imuBuffers.accelX, "#4fc3f7");
  drawSparkline("spark-accel-y", imuBuffers.accelY, "#81c784");
  drawSparkline("spark-accel-z", imuBuffers.accelZ, "#ffb74d");
  drawSparkline("spark-gyro-x", imuBuffers.gyroX, "#4fc3f7");
  drawSparkline("spark-gyro-y", imuBuffers.gyroY, "#81c784");
  drawSparkline("spark-gyro-z", imuBuffers.gyroZ, "#ffb74d");
  // Update value displays
  setText("val-accel-x", axG.toFixed(3) + " g");
  setText("val-accel-y", ayG.toFixed(3) + " g");
  setText("val-accel-z", azG.toFixed(3) + " g");
  setText("val-gyro-x", gxDS.toFixed(1) + " °/s");
  setText("val-gyro-y", gyDS.toFixed(1) + " °/s");
  setText("val-gyro-z", gzDS.toFixed(1) + " °/s");
  // Pitch / roll from accelerometer
  // Pro Controller IMU: X = lateral (left/right), Y = longitudinal (front/back)
  // Pitch (tilt forward/back) → Y axis changes; Roll (tilt sideways) → X axis changes
  const pitch = (Math.atan2(axG, azG) * 180) / Math.PI;
  const roll = (Math.atan2(ayG, azG) * 180) / Math.PI;
  setText("imu-pitch", pitch.toFixed(1) + "°");
  setText("imu-roll", roll.toFixed(1) + "°");
  // Yaw from gyro integration (approximate, display raw accumulated)
  setText("imu-yaw", (gzDS).toFixed(1) + "°");
  updateHorizon(pitch, roll);
}

function updateHorizon(pitch, roll) {
  const rot = el("horizon-rotate");
  const ladder = el("horizon-ladder");
  const rollInd = el("horizon-roll-indicator");
  if (rot) {
    // Rotate around center (70, 42) by -roll
    rot.setAttribute("transform", `rotate(${-roll} 70 42)`);
  }
  if (ladder) {
    // Translate the pitch ladder: positive pitch moves ground down (ladder up)
    const scale = 0.6; // pixels per degree
    ladder.setAttribute("transform", `translate(0 ${pitch * scale})`);
  }
  if (rollInd) {
    rollInd.setAttribute("transform", `rotate(${roll} 70 42)`);
  }
}

// IMU enable / gyro aim toggles
el("imu-enable")?.addEventListener("change", (e) => {
  imuEnabled = e.target.checked;
  if (invoke) {
    invoke("enable_imu", { enabled: imuEnabled }).catch((err) =>
      handleError("enable_imu failed", err)
    );
  }
  appendLog(`[IMU] ${imuEnabled ? "enabled" : "disabled"}`);
});

el("gyro-aim-mode")?.addEventListener("change", (e) => {
  gyroAimEnabled = e.target.checked;
  if (invoke) {
    invoke("set_gyro_aim", {
      enabled: gyroAimEnabled,
      sensitivity: gyroSensitivity,
      deadzone: 2.0,
    }).catch((err) => handleError("set_gyro_aim failed", err));
  }
  appendLog(`[Gyro Aim] ${gyroAimEnabled ? "enabled" : "disabled"}`);
});

el("gyro-sensitivity")?.addEventListener("input", (e) => {
  const val = parseInt(e.target.value, 10);
  gyroSensitivity = val / 1000;
  setText("gyro-sensitivity-val", gyroSensitivity.toFixed(3));
  if (invoke && gyroAimEnabled) {
    invoke("set_gyro_aim", {
      enabled: true,
      sensitivity: gyroSensitivity,
      deadzone: 2.0,
    }).catch((err) => handleError("set_gyro_aim failed", err));
  }
});

// =============================================================================
// Player LED controls
// =============================================================================
const LED_PRESET_PATTERNS = {
  solid: 0,
  chase: 1,
  blink: 2,
  pulse: 3,
};
let currentLedPreset = "solid";
let ledPatternTimer = null;
let ledPatternStep = 0;

// buildLedMask and getCheckedLedIndices imported from ./utils.js

function updateLedVisuals(activeMask) {
  const mask = activeMask !== undefined ? activeMask : buildLedMask();
  const animating = currentLedPreset !== "solid" && ledPatternTimer !== null;
  for (let i = 1; i <= 4; i++) {
    const ind = el(`led-${i}`);
    if (ind) {
      const on = (mask >> (i - 1)) & 1;
      ind.classList.toggle("active", !!on);
      ind.classList.toggle("flashing", false); // we handle animation via JS now
      ind.classList.toggle("animating", !!on && animating);
    }
  }
}

function sendLedMask(mask, flashPattern) {
  if (invoke) {
    invoke("set_player_lights", { ledMask: mask, flashPattern: flashPattern || 0 }).catch((err) =>
      handleError("set_player_lights failed", err)
    );
  }
}

function stopLedPattern() {
  if (ledPatternTimer) {
    clearInterval(ledPatternTimer);
    ledPatternTimer = null;
    ledPatternStep = 0;
  }
}

function startLedPattern() {
  stopLedPattern();
  const checked = getCheckedLedIndices();
  if (checked.length === 0) return;

  if (currentLedPreset === "chase") {
    // Chase: light up one LED at a time, cycling through checked LEDs
    ledPatternStep = 0;
    const step = () => {
      const idx = checked[ledPatternStep % checked.length];
      const mask = 1 << (idx - 1);
      sendLedMask(mask, 0);
      updateLedVisuals(mask);
      ledPatternStep++;
    };
    step();
    ledPatternTimer = setInterval(step, 250);
  } else if (currentLedPreset === "blink") {
    // Blink: use the hardware flash bit (flashPattern=2) — the controller
    // handles the blinking natively, no software sequencing needed.
    stopLedPattern();
    const fullMask = buildLedMask();
    updateLedVisuals(fullMask);
    sendLedMask(fullMask, 2);
  } else if (currentLedPreset === "pulse") {
    // Pulse: all checked LEDs on, then all off, slow cycle (like breathing)
    ledPatternStep = 0;
    const fullMask = buildLedMask();
    const step = () => {
      const mask = ledPatternStep % 2 === 0 ? fullMask : 0;
      sendLedMask(mask, 0);
      updateLedVisuals(mask);
      ledPatternStep++;
    };
    step();
    ledPatternTimer = setInterval(step, 700);
  }
}

function sendPlayerLights() {
  const ledMask = buildLedMask();
  if (currentLedPreset === "solid") {
    stopLedPattern();
    updateLedVisuals(ledMask);
    sendLedMask(ledMask, 0);
  } else {
    // For chase/blink/pulse, start the pattern sequencer
    // (blink uses hardware flash internally)
    startLedPattern();
  }
}

for (let i = 1; i <= 4; i++) {
  const tog = el(`led-toggle-${i}`);
  if (tog) tog.addEventListener("change", sendPlayerLights);
}

["solid", "chase", "blink", "pulse"].forEach((preset) => {
  const btn = el(`led-preset-${preset}`);
  if (!btn) return;
  btn.addEventListener("click", () => {
    document
      .querySelectorAll(".led-presets .preset-btn")
      .forEach((b) => b.classList.remove("active"));
    btn.classList.add("active");
    currentLedPreset = preset;
    sendPlayerLights();
  });
});

function syncPlayerLeds(data) {
  // Backend echoes the LED state. Do NOT touch checkboxes or indicators —
  // the pattern sequencer manages visuals. This event is only useful for
  // initial state sync from the backend, which we don't need here.
}

// =============================================================================
// Home light controls
// =============================================================================
function updateHomeRingPreview() {
  const ring = el("home-ring");
  const pattern = el("home-pattern")?.value || "solid";
  const brightness = parseInt(el("home-brightness")?.value || "50", 10);
  if (!ring) return;
  ring.classList.remove("active", "breathing", "blink", "fade", "wave");
  if (brightness > 0) {
    if (pattern === "solid") {
      ring.classList.add("active");
    } else if (pattern === "breathing") {
      ring.classList.add("breathing");
    } else if (pattern === "blink") {
      ring.classList.add("blink");
    } else if (pattern === "fade") {
      ring.classList.add("fade");
    } else if (pattern === "wave") {
      ring.classList.add("wave");
    }
  }
}

el("home-brightness")?.addEventListener("input", (e) => {
  setText("home-brightness-val", e.target.value + "%");
});
el("home-brightness")?.addEventListener("change", () => sendHomeLight(true));

el("home-pattern")?.addEventListener("change", () => sendHomeLight(true));

el("home-duration")?.addEventListener("input", (e) => {
  setText("home-duration-val", e.target.value + " ms");
});

el("home-cycles")?.addEventListener("input", (e) => {
  const v = parseInt(e.target.value, 10);
  setText("home-cycles-val", v === 0 ? "∞" : String(v));
});

el("btn-home-test")?.addEventListener("click", () => sendHomeLight(true));
el("btn-home-off")?.addEventListener("click", () => sendHomeLight(false));

// Map numeric pattern index back to string for the dropdown.
const HOME_LIGHT_PATTERN_NAMES = ["solid", "breathing", "blink", "fade", "wave"];

function syncHomeLight(data) {
  const brightness = data.brightness ?? 50;
  let pattern = data.pattern ?? "solid";
  if (typeof pattern === "number") pattern = HOME_LIGHT_PATTERN_NAMES[pattern] || "solid";
  const brightSlider = el("home-brightness");
  const patternSel = el("home-pattern");
  if (brightSlider) brightSlider.value = brightness;
  if (patternSel) patternSel.value = pattern;
  setText("home-brightness-val", brightness + "%");
  updateHomeRingPreview();
}

function sendHomeLight(enabled = true) {
  const brightness = parseInt(el("home-brightness")?.value || "50", 10);
  const pattern = el("home-pattern")?.value || "solid";
  updateHomeRingPreview();
  if (invoke) {
    invoke("set_home_light", { enabled, brightness, pattern }).catch((err) =>
      handleError("set_home_light failed", err)
    );
  }
  appendLog(`[Home Light] ${enabled ? "on" : "off"}: ${brightness}% ${pattern}`);
}

// =============================================================================
// Calibration panel
// =============================================================================
const stickTrails = { left: [], right: [] };
const TRAIL_MAX = 50;

function updateStickCalibration(ev) {
  // Backend sends IpcEvent::CalibrationData { stick: StickCalibration, imu: ImuCalibration }
  // StickCalibration has flat fields: left_center_x, left_min_x, left_max_x, etc.
  const cal = ev.stick || ev.calibration || {};

  // Populate factory calibration data (flat field names → flat HTML IDs)
  populateFactoryData(cal);

  // Update source badge + valid indicator
  updateCalibrationSource(cal.source || "default");
  updateCalibrationValid(cal.valid !== false);

  // Update IMU calibration display
  if (ev.imu) {
    populateImuCalibration(ev.imu);
  }

  // Also update the stick visualizer dots from the current controller state
  // (the calibration event doesn't include live stick positions, but the
  // ControllerState events do — we update dots there).
}

function updateStickCalDotFromState(side, stick) {
  const dot = el(`stick-${side}-dot`);
  const trailEl = el(`stick-${side}-trail`);
  if (!dot) return;
  const cx = stick.x ?? 0;
  const cy = stick.y ?? 0;
  const rawX = stick.raw_x ?? 0;
  const rawY = stick.raw_y ?? 0;
  // Map -1..1 to SVG coords (6..114, center 60)
  const svgX = 60 + Math.max(-1, Math.min(1, cx)) * 54;
  const svgY = 60 - Math.max(-1, Math.min(1, cy)) * 54; // invert Y for SVG
  dot.setAttribute("cx", svgX);
  dot.setAttribute("cy", svgY);
  // Update readouts
  setText(`stick-${side}-raw-x`, String(rawX));
  setText(`stick-${side}-raw-y`, String(rawY));
  setText(`stick-${side}-cal-x`, cx.toFixed(3));
  setText(`stick-${side}-cal-y`, cy.toFixed(3));
  // Center delta (raw - 0x800 = drift offset)
  const offX = rawX - 0x800;
  const offY = rawY - 0x800;
  setText(`stick-${side}-offset-x`, String(offX));
  setText(`stick-${side}-offset-y`, String(offY));
  // Trail
  stickTrails[side].push(`${svgX},${svgY}`);
  while (stickTrails[side].length > TRAIL_MAX) stickTrails[side].shift();
  if (trailEl) {
    trailEl.setAttribute("points", stickTrails[side].join(" "));
  }
}

function updateStickCalDot(side, stick) {
  const dot = el(`stick-${side}-dot`);
  const trailEl = el(`stick-${side}-trail`);
  if (!dot) return;
  // stick.x / stick.y in -1..1 (calibrated), or raw
  const cx = stick.cal_x ?? stick.calX ?? stick.x ?? 0;
  const cy = stick.cal_y ?? stick.calY ?? stick.y ?? 0;
  const rawX = stick.raw_x ?? stick.rawX ?? 0;
  const rawY = stick.raw_y ?? stick.rawY ?? 0;
  // Map -1..1 to SVG coords (6..114, center 60)
  const svgX = 60 + Math.max(-1, Math.min(1, cx)) * 54;
  const svgY = 60 - Math.max(-1, Math.min(1, cy)) * 54; // invert Y for SVG
  dot.setAttribute("cx", svgX);
  dot.setAttribute("cy", svgY);
  // Update readouts
  setText(`stick-${side}-raw-x`, String(rawX));
  setText(`stick-${side}-raw-y`, String(rawY));
  setText(`stick-${side}-cal-x`, cx.toFixed(3));
  setText(`stick-${side}-cal-y`, cy.toFixed(3));
  // Center offset
  const offX = stick.offset_x ?? stick.offsetX ?? (rawX - (stick.center_x ?? stick.centerX ?? 0));
  const offY = stick.offset_y ?? stick.offsetY ?? (rawY - (stick.center_y ?? stick.centerY ?? 0));
  setText(`stick-${side}-offset-x`, String(offX));
  setText(`stick-${side}-offset-y`, String(offY));
  // Trail
  stickTrails[side].push(`${svgX},${svgY}`);
  while (stickTrails[side].length > TRAIL_MAX) stickTrails[side].shift();
  if (trailEl) {
    trailEl.setAttribute("points", stickTrails[side].join(" "));
  }
}

function populateFactoryData(cal) {
  // cal is a StickCalibration with flat fields: left_center_x, left_min_x, etc.
  // HTML IDs are: fac-left-max-x, fac-left-min-x, fac-left-center-x, etc.
  const fields = [
    ["left", "max", "x", "left_max_x"],
    ["left", "min", "x", "left_min_x"],
    ["left", "center", "x", "left_center_x"],
    ["left", "max", "y", "left_max_y"],
    ["left", "min", "y", "left_min_y"],
    ["left", "center", "y", "left_center_y"],
    ["right", "max", "x", "right_max_x"],
    ["right", "min", "x", "right_min_x"],
    ["right", "center", "x", "right_center_x"],
    ["right", "max", "y", "right_max_y"],
    ["right", "min", "y", "right_min_y"],
    ["right", "center", "y", "right_center_y"],
  ];
  fields.forEach(([side, field, axis, calField]) => {
    const elem = el(`fac-${side}-${field}-${axis}`);
    if (elem && cal[calField] !== undefined) {
      elem.textContent = String(cal[calField]);
    }
  });
}

// =============================================================================
// Advanced calibration display — source badge, valid indicator, status, IMU
// =============================================================================

function updateCalibrationSource(source) {
  const badge = el("cal-source-badge");
  if (!badge) return;
  const label = source.charAt(0).toUpperCase() + source.slice(1);
  badge.textContent = label;
  badge.className = "cal-source-badge cal-source-" + source;
  // Also mirror to the IMU source badge if separate
  const imuBadge = el("imu-cal-source-badge");
  if (imuBadge) {
    imuBadge.textContent = label;
    imuBadge.className = "cal-source-badge cal-source-" + source;
  }
}

function updateCalibrationValid(valid) {
  const indicator = el("cal-valid-indicator");
  if (!indicator) return;
  indicator.textContent = valid ? "✓ Valid" : "✗ Invalid";
  indicator.className = "cal-valid-" + (valid ? "yes" : "no");
}

function updateCalibrationStatus(status) {
  // status = { drift_status, noise_floor, adaptive_deadzone, center_offset, center_locked, gate_calibrated }
  const driftEl = el("cal-drift-status");
  const driftStatus = (status.drift_status || "UNKNOWN").toString();
  if (driftEl) {
    driftEl.textContent = driftStatus.toUpperCase();
    driftEl.className = "cal-status-value cal-status-" + driftStatus.toLowerCase();
  }
  setText("cal-noise-floor", status.noise_floor != null ? status.noise_floor.toFixed(4) : "—");
  setText("cal-adaptive-deadzone", status.adaptive_deadzone != null ? status.adaptive_deadzone.toFixed(4) : "—");
  setText("cal-center-offset-x", status.center_offset?.[0] != null ? (status.center_offset[0] >= 0 ? "+" : "") + status.center_offset[0].toFixed(4) : "0.0000");
  setText("cal-center-offset-y", status.center_offset?.[1] != null ? (status.center_offset[1] >= 0 ? "+" : "") + status.center_offset[1].toFixed(4) : "0.0000");
  const centerLockedEl = el("cal-center-locked");
  if (centerLockedEl) {
    centerLockedEl.textContent = status.center_locked ? "YES" : "NO";
    centerLockedEl.className = "cal-status-value " + (status.center_locked ? "cal-status-pass" : "cal-status-unknown");
  }
  const gateCalEl = el("cal-gate-calibrated");
  if (gateCalEl) {
    gateCalEl.textContent = status.gate_calibrated ? "YES" : "NO";
    gateCalEl.className = "cal-status-value " + (status.gate_calibrated ? "cal-status-pass" : "cal-status-unknown");
  }
}

function populateImuCalibration(imu) {
  // Update IMU source badge
  const source = imu.source || "default";
  const badge = el("imu-cal-source-badge");
  if (badge) {
    badge.textContent = source.charAt(0).toUpperCase() + source.slice(1);
    badge.className = "cal-source-badge cal-source-" + source;
  }
  setText("imu-cal-accel-origin-x", String(imu.accel_origin?.[0] ?? 0));
  setText("imu-cal-accel-origin-y", String(imu.accel_origin?.[1] ?? 0));
  setText("imu-cal-accel-origin-z", String(imu.accel_origin?.[2] ?? 0));
  setText("imu-cal-accel-sens-x", String(imu.accel_sensitivity?.[0] ?? 0));
  setText("imu-cal-accel-sens-y", String(imu.accel_sensitivity?.[1] ?? 0));
  setText("imu-cal-accel-sens-z", String(imu.accel_sensitivity?.[2] ?? 0));
  setText("imu-cal-gyro-origin-x", String(imu.gyro_origin?.[0] ?? 0));
  setText("imu-cal-gyro-origin-y", String(imu.gyro_origin?.[1] ?? 0));
  setText("imu-cal-gyro-origin-z", String(imu.gyro_origin?.[2] ?? 0));
  setText("imu-cal-gyro-sens-x", String(imu.gyro_sensitivity?.[0] ?? 0));
  setText("imu-cal-gyro-sens-y", String(imu.gyro_sensitivity?.[1] ?? 0));
  setText("imu-cal-gyro-sens-z", String(imu.gyro_sensitivity?.[2] ?? 0));
}

// =============================================================================
// Response curve selector + preview
// =============================================================================

function setupResponseCurveControls() {
  const curveSelect = el("response-curve-type");
  const powerSlider = el("response-curve-power");
  const powerVal = el("response-curve-power-val");
  const previewCanvas = el("response-curve-preview");

  if (curveSelect) {
    curveSelect.addEventListener("change", (e) => {
      const curveType = e.target.value;
      const power = parseFloat(powerSlider?.value || "1300") / 1000;
      if (invoke) {
        invoke("set_response_curve", { curveType, power }).catch((err) =>
          handleError("set_response_curve failed", err)
        );
      }
      drawResponseCurve(previewCanvas, curveType, power);
    });
  }

  if (powerSlider) {
    powerSlider.addEventListener("input", (e) => {
      const power = parseInt(e.target.value, 10) / 1000;
      if (powerVal) powerVal.textContent = power.toFixed(3);
      drawResponseCurve(previewCanvas, curveSelect?.value || "exponential", power);
    });
    // Send final value on release
    powerSlider.addEventListener("change", (e) => {
      const power = parseInt(e.target.value, 10) / 1000;
      if (invoke) {
        invoke("set_response_curve", { curveType: curveSelect?.value || "exponential", power }).catch((err) =>
          handleError("set_response_curve failed", err)
        );
      }
    });
  }

  // Initial draw
  drawResponseCurve(previewCanvas, "exponential", 1.3);
}

function drawResponseCurve(canvas, type, power) {
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);

  // Draw grid
  ctx.strokeStyle = "rgba(120, 200, 255, 0.12)";
  ctx.lineWidth = 0.5;
  for (let i = 0; i <= 4; i++) {
    const p = (i / 4) * w;
    ctx.beginPath();
    ctx.moveTo(p, 0);
    ctx.lineTo(p, h);
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(0, (i / 4) * h);
    ctx.lineTo(w, (i / 4) * h);
    ctx.stroke();
  }

  // Draw reference linear line (faint)
  ctx.strokeStyle = "rgba(138, 150, 168, 0.25)";
  ctx.lineWidth = 1;
  ctx.setLineDash([3, 3]);
  ctx.beginPath();
  ctx.moveTo(0, h);
  ctx.lineTo(w, 0);
  ctx.stroke();
  ctx.setLineDash([]);

  // Draw curve
  ctx.strokeStyle = "#78c8ff";
  ctx.lineWidth = 2;
  ctx.beginPath();
  for (let px = 0; px <= w; px++) {
    const input = px / w;
    let output;
    if (type === "linear") output = input;
    else if (type === "exponential") output = Math.pow(input, power);
    else if (type === "s-curve") output = input * input * (3 - 2 * input);
    else if (type === "bezier") {
      // Cubic bezier with control points
      const t = input;
      const one_t = 1 - t;
      output = 3 * one_t * one_t * t * 0.9 + 3 * one_t * t * t * 0.1 + t * t * t;
    } else output = input;
    const py = h - output * h;
    if (px === 0) ctx.moveTo(px, py);
    else ctx.lineTo(px, py);
  }
  ctx.stroke();

  // Glow effect
  ctx.shadowColor = "rgba(120, 200, 255, 0.5)";
  ctx.shadowBlur = 6;
  ctx.stroke();
  ctx.shadowBlur = 0;
}

// =============================================================================
// Gate calibration
// =============================================================================

function setupGateCalibration() {
  const btn = el("btn-calibrate-gate");
  const progress = el("gate-cal-progress");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    if (!invoke) return;
    btn.disabled = true;
    appendLog("[CAL] Gate calibration started — sweep stick around the edge");
    if (progress) progress.style.display = "block";
    try {
      await invoke("start_gate_calibration");
      // Poll for completion
      const poll = setInterval(async () => {
        try {
          const done = await invoke("get_gate_calibration_status");
          if (done) {
            clearInterval(poll);
            btn.disabled = false;
            if (progress) progress.style.display = "none";
            appendLog("[CAL] Gate calibration complete");
          }
        } catch (e) {
          clearInterval(poll);
          btn.disabled = false;
          if (progress) progress.style.display = "none";
        }
      }, 500);
    } catch (e) {
      btn.disabled = false;
      if (progress) progress.style.display = "none";
      appendLog("[ERR] Gate calibration failed: " + e, "warn-line");
    }
  });
}

// =============================================================================
// Advanced calibration toggles
// =============================================================================

function setupCalibrationToggles() {
  const toggles = [
    "adaptive-deadzone-toggle",
    "center-auto-cal-toggle",
    "drift-detection-toggle",
    "gate-calibration-toggle",
  ];
  toggles.forEach((id) => {
    const checkbox = el(id);
    if (!checkbox) return;
    checkbox.addEventListener("change", (e) => {
      const option = id.replace("-toggle", "");
      if (invoke) {
        invoke("set_calibration_option", { option, enabled: e.target.checked }).catch((err) =>
          handleError("set_calibration_option failed", err)
        );
      }
      appendLog(`[CAL] ${option}: ${e.target.checked ? "enabled" : "disabled"}`);
    });
  });
}

// Boot setup for calibration controls
setupResponseCurveControls();
setupGateCalibration();
setupCalibrationToggles();

el("btn-recalibrate")?.addEventListener("click", async () => {
  appendLog("[CAL] Recalibration started — release sticks");
  if (invoke) {
    try {
      await invoke("recalibrate_sticks");
      appendLog("[CAL] Recalibration complete");
    } catch (e) {
      appendLog("[ERR] recalibrate_sticks failed: " + e, "warn-line");
    }
  }
});

el("btn-reset-factory")?.addEventListener("click", async () => {
  appendLog("[CAL] Resetting to factory calibration");
  if (invoke) {
    try {
      await invoke("reset_factory_calibration");
      appendLog("[CAL] Factory calibration restored");
    } catch (e) {
      appendLog("[ERR] reset_factory_calibration failed: " + e, "warn-line");
    }
  }
});

el("btn-clear-drift")?.addEventListener("click", () => {
  stickTrails.left = [];
  stickTrails.right = [];
  const lt = el("stick-left-trail");
  const rt = el("stick-right-trail");
  if (lt) lt.setAttribute("points", "");
  if (rt) rt.setAttribute("points", "");
  appendLog("[CAL] Drift trails cleared");
  if (invoke) {
    invoke("clear_drift").catch((err) => handleError("clear_drift failed", err));
  }
});

// =============================================================================
// Diagnostics — device info
// =============================================================================
const CONTROLLER_TYPES = {
  1: "Left Joy-Con",
  2: "Right Joy-Con",
  3: "Pro Controller",
};

function updateDeviceInfo(info) {
  setText("dev-type", CONTROLLER_TYPES[info.controller_type ?? info.type] || "Unknown");
  setText("dev-firmware", info.firmware_version || info.firmware || "—");
  setText("dev-mac", info.mac_address || info.mac || "—");
  const connStr = info.connection || info.transport || "—";
  setText("dev-connection", connStr);

  // Update the transport label for the status chip.
  currentTransport = connStr !== "—" ? connStr : null;
  // Refresh the chip label if we're already connected.
  const chip = el("connection-chip");
  if (chip && chip.classList.contains("connected")) {
    el("conn-label").textContent = `Connected · ${currentTransport}`;
  }
  // Show/hide the "Switch to BT" button based on transport.
  updateBtSwitchButton();

  // Track connection type for NFC/Amiibo availability.
  // NFC/IR is a Broadcom MCU feature — not available over USB (STM32 bridge).
  const isUsb = connStr.toLowerCase() === "usb";
  const nfcNotice = el("nfc-usb-notice");
  const nfcBody = el("nfc-controls-body");
  if (nfcNotice) nfcNotice.hidden = !isUsb;
  if (nfcBody) {
    if (isUsb) nfcBody.classList.add("usb-disabled");
    else nfcBody.classList.remove("usb-disabled");
  }
  // SPI flash info if present
  if (info.spi) {
    setText("spi-cal-status", info.spi.calibration ? "Present" : "Not stored");
    // Serial number is often blank on Pro Controllers — this is normal
    const serial = info.spi.serial;
    setText("spi-serial", serial && serial.length > 0 ? serial : "Not stored");
    // Body color — show "Not stored" if all zeros (SPI uninitialized)
    const bodySw = el("spi-body-color");
    const gripSw = el("spi-grip-color");
    if (bodySw) {
      if (info.spi.body_color && info.spi.body_color !== "rgb(0,0,0)") {
        bodySw.style.background = info.spi.body_color;
        bodySw.title = info.spi.body_color;
      } else {
        bodySw.style.background = "rgba(120, 200, 255, 0.08)";
        bodySw.title = "Not stored (SPI flash uninitialized)";
      }
    }
    if (gripSw) {
      if (info.spi.grip_color && info.spi.grip_color !== "rgb(0,0,0)") {
        gripSw.style.background = info.spi.grip_color;
        gripSw.title = info.spi.grip_color;
      } else {
        gripSw.style.background = "rgba(120, 200, 255, 0.08)";
        gripSw.title = "Not stored (SPI flash uninitialized)";
      }
    }
    // Button color — show "Not stored" if all zeros (SPI uninitialized)
    const btnColorSw = el("spi-button-color-swatch");
    if (info.spi.button_color && info.spi.button_color !== "rgb(0,0,0)") {
      setText("spi-button-color", info.spi.button_color);
      if (btnColorSw) {
        btnColorSw.style.display = "";
        btnColorSw.style.background = info.spi.button_color;
        btnColorSw.title = info.spi.button_color;
      }
    } else {
      setText("spi-button-color", "Not Stored");
      if (btnColorSw) btnColorSw.style.display = "none";
    }
    // SPI colors active flag — indicates whether the controller uses SPI-stored colors
    const spiColorsEl = el("spi-colors-active");
    if (spiColorsEl) {
      if (info.spi.use_spi_colors === true) {
        spiColorsEl.textContent = "Active";
        spiColorsEl.classList.add("ok");
      } else if (info.spi.use_spi_colors === false) {
        spiColorsEl.textContent = "Inactive";
        spiColorsEl.classList.remove("ok");
      } else {
        spiColorsEl.textContent = "Unknown";
        spiColorsEl.classList.remove("ok");
      }
    }
  } else {
    // SPI data not yet read — show pending state
    setText("spi-cal-status", "Reading…");
    setText("spi-serial", "Reading…");
  }
}

// =============================================================================
// Connection quality
// =============================================================================
const signalHistory = [];
const SIGNAL_HISTORY_MAX = 300; // ~30s at 10Hz
const latencyAvg = new RollingAverage(1000);
const reportRateAvg = new RollingAverage(1000);
const packetLossAvg = new RollingAverage(1000);

function updateConnectionQuality(quality) {
  const latency = quality.latency_ms ?? quality.latency ?? 0;
  const rate = quality.report_rate_hz ?? quality.report_rate ?? 0;
  const packetLoss = quality.packet_loss_rate ?? quality.packet_loss_pct ?? quality.packet_loss ?? 0;
  // Smooth with 1s rolling average
  latencyAvg.push(latency);
  reportRateAvg.push(rate);
  packetLossAvg.push(packetLoss);
  const latS = latencyAvg.avg();
  const rateS = reportRateAvg.avg();
  const lossS = packetLossAvg.avg();
  setText("conn-latency", (latS !== null ? latS : latency).toFixed(1) + " ms");
  setText("conn-report-rate", (rateS !== null ? rateS : rate).toFixed(0) + " Hz");
  setText("conn-packet-loss", (lossS !== null ? lossS : packetLoss).toFixed(1) + "%");
  setText("conn-total-packets", String(quality.total_packets ?? 0));
  setText("conn-dropped", String(quality.dropped ?? 0));
  setText("conn-retries", String(quality.retries ?? 0));
  // Latency bar fill (0-100ms scale) — use smoothed value
  const fill = el("latency-bar-fill");
  if (fill) {
    const latForBar = latS !== null ? latS : latency;
    const pct = Math.min(100, (latForBar / 100) * 100);
    fill.style.width = pct + "%";
  }
  // Rate indicator dots — use smoothed value
  const dots = el("rate-indicator");
  if (dots) {
    const rateForDots = rateS !== null ? rateS : rate;
    let level = 1;
    if (rateForDots >= 120) level = 5;
    else if (rateForDots >= 100) level = 4;
    else if (rateForDots >= 60) level = 3;
    else if (rateForDots >= 30) level = 2;
    const dotEls = dots.querySelectorAll(".rate-dot");
    dotEls.forEach((d, i) => d.classList.toggle("active", i < level));
  }
  // Signal history — use smoothed latency
  signalHistory.push(latS !== null ? latS : latency);
  while (signalHistory.length > SIGNAL_HISTORY_MAX) signalHistory.shift();
  drawSignalHistory();
}

function drawSignalHistory() {
  const canvas = el("signal-history-canvas");
  if (!canvas) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  if (signalHistory.length < 2) return;
  let max = 50;
  for (const v of signalHistory) {
    if (v > max) max = v;
  }
  max = Math.max(max, 10);
  ctx.strokeStyle = "#4fc3f7";
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  for (let i = 0; i < signalHistory.length; i++) {
    const x = (i / (SIGNAL_HISTORY_MAX - 1)) * w;
    const y = h - (signalHistory[i] / max) * h;
    if (i === 0) ctx.moveTo(x, y);
    else ctx.lineTo(x, y);
  }
  ctx.stroke();
}

// =============================================================================
// Rumble test panel
// =============================================================================
function updateRumbleVisualizer() {
  const ampL = parseInt(el("rumble-amp-left")?.value || "0", 10);
  const ampR = parseInt(el("rumble-amp-right")?.value || "0", 10);
  const barL = el("rumble-bar-left");
  const barR = el("rumble-bar-right");
  if (barL) barL.style.height = ampL + "%";
  if (barR) barR.style.height = ampR + "%";
  setText("rumble-bar-left-val", ampL + "%");
  setText("rumble-bar-right-val", ampR + "%");
}

el("rumble-amp-left")?.addEventListener("input", (e) => {
  setText("rumble-amp-left-val", e.target.value + "%");
  updateRumbleVisualizer();
});

el("rumble-amp-right")?.addEventListener("input", (e) => {
  setText("rumble-amp-right-val", e.target.value + "%");
  updateRumbleVisualizer();
});

el("rumble-freq")?.addEventListener("input", (e) => {
  setText("rumble-freq-val", e.target.value + " Hz");
});

el("btn-rumble-test")?.addEventListener("click", async () => {
  const enabled = el("rumble-enable")?.checked ?? true;
  if (!enabled) {
    appendLog("[Rumble] Vibration disabled — toggle enable first", "warn-line");
    return;
  }
  const leftAmp = parseInt(el("rumble-amp-left")?.value || "50", 10) / 100;
  const rightAmp = parseInt(el("rumble-amp-right")?.value || "50", 10) / 100;
  const freq = parseInt(el("rumble-freq")?.value || "300", 10);
  if (invoke) {
    try {
      // Ensure vibration is enabled on the controller (subcommand 0x48).
      // The device loop's rumble refresh only fires if vibration_enabled
      // is true, which is set by enable_vibration. Without this, the
      // single rumble report from send_rumble is too brief to notice.
      await invoke("enable_vibration", { enabled: true });
      await invoke("send_rumble", {
        leftAmp,
        rightAmp,
        leftFreq: freq,
        rightFreq: freq,
      });
      appendLog(`[Rumble] test: L=${(leftAmp * 100).toFixed(0)}% R=${(rightAmp * 100).toFixed(0)}% ${freq}Hz`);
    } catch (e) {
      appendLog("[ERR] send_rumble failed: " + e, "warn-line");
    }
  }
});

// Wire the "Vibration Enable" toggle to the backend so the device loop's
// rumble refresh starts/stops. Without this, the toggle only affected the
// "Test Rumble" button's local guard, not the actual controller state.
el("rumble-enable")?.addEventListener("change", async (e) => {
  if (!invoke) return;
  try {
    await invoke("enable_vibration", { enabled: e.target.checked });
    appendLog(`[Rumble] vibration ${e.target.checked ? "enabled" : "disabled"}`);
    if (!e.target.checked) {
      // When disabling, also zero out the rumble amplitudes so the motors stop.
      await invoke("send_rumble", {
        leftAmp: 0,
        rightAmp: 0,
        leftFreq: 160,
        rightFreq: 160,
      });
    }
  } catch (err) {
    appendLog("[ERR] enable_vibration failed: " + err, "warn-line");
  }
});

el("btn-rumble-stop")?.addEventListener("click", async () => {
  if (invoke) {
    try {
      await invoke("send_rumble", {
        leftAmp: 0,
        rightAmp: 0,
        leftFreq: 160,
        rightFreq: 160,
      });
      // Disable vibration on the controller so the device loop stops
      // sending rumble refresh reports.
      await invoke("enable_vibration", { enabled: false });
      appendLog("[Rumble] stopped");
    } catch (e) {
      appendLog("[ERR] stop rumble failed: " + e, "warn-line");
    }
  }
});

// =============================================================================
// Enhanced battery panel
// =============================================================================
function updateBatteryEnhanced(state) {
  const rawPct = state.percent ?? state.battery_percent ?? 0;
  // Median filter for stable display
  batteryEnhAvg.push(rawPct);
  const med = batteryEnhAvg.median();
  const pct = Math.max(0, Math.min(100, med !== null ? med : rawPct));
  const charging = state.charging ?? false;
  setText("battery-pct-enhanced", Math.round(pct) + "%");
  const statusEl = el("battery-status-enhanced");
  const levelEl = el("battery-level-enhanced");
  const sourceEl = el("battery-power-source");
  if (statusEl) {
    statusEl.textContent = charging ? "Charging" : pct < 20 ? "Low" : "Discharging";
    statusEl.classList.toggle("charging", charging);
    statusEl.classList.toggle("low", pct < 20);
  }
  if (levelEl) {
    levelEl.style.width = pct + "%";
    levelEl.classList.toggle("low", pct < 20);
    levelEl.classList.toggle("charging", charging);
  }
  if (sourceEl) {
    sourceEl.textContent = charging ? "USB-C" : "Battery";
  }
  setText("battery-health", state.health || "—");
}

el("battery-low-threshold")?.addEventListener("input", (e) => {
  setText("battery-low-threshold-val", e.target.value + "%");
});

// =============================================================================
// Helper — safe text setter
// =============================================================================
function setText(id, text) {
  const node = el(id);
  if (node) node.textContent = text;
}

// =============================================================================
// NFC/IR functions
// =============================================================================
function updateNfcTagDisplay(tag) {
  if (!tag) {
    if (el("nfc-tag-info")) el("nfc-tag-info").style.display = "none";
    return;
  }
  if (el("nfc-tag-info")) el("nfc-tag-info").style.display = "block";
  setText("nfc-tag-uid", tag.uid ? tag.uid.map(b => b.toString(16).padStart(2, "0")).join(":") : "—");
  setText("nfc-tag-type", tag.tag_type !== undefined ? `0x${tag.tag_type.toString(16)}` : "—");
  setText("nfc-tag-amiibo", tag.is_amiibo ? "Yes" : "No");
  setText("nfc-tag-size", `${tag.data ? tag.data.length : 0} bytes`);
  if (el("nfc-tag-raw") && tag.data) {
    const hex = tag.data.map(b => b.toString(16).padStart(2, "0")).join(" ");
    // Chunk into 16-byte rows
    const rows = [];
    for (let i = 0; i < hex.length; i += 48) {
      rows.push(hex.slice(i, i + 48));
    }
    el("nfc-tag-raw").textContent = rows.join("\n");
  }
}

// NFC mode select handler
if (el("nfc-mode-select")) {
  el("nfc-mode-select").addEventListener("change", (e) => {
    const mode = parseInt(e.target.value, 10);
    invoke("set_nfc_mode", { mode }).catch(err => handleError("set_nfc_mode failed", err));
    setText("nfc-status-text", mode === 0 ? "Inactive" : "Active");
  });
}

// NFC scan button
if (el("btn-nfc-scan")) {
  el("btn-nfc-scan").addEventListener("click", () => {
    invoke("get_nfc_data").then(tag => {
      if (tag) updateNfcTagDisplay(tag);
      else setText("nfc-status-text", "No tag found");
    }).catch(err => handleError("NFC scan failed", err));
  });
}

// NFC clear button
if (el("btn-nfc-clear")) {
  el("btn-nfc-clear").addEventListener("click", () => {
    if (el("nfc-tag-info")) el("nfc-tag-info").style.display = "none";
    setText("nfc-status-text", "Cleared");
  });
}

// =============================================================================
// ViGEmBus Setup panel
// =============================================================================
function updateVigemBusStatus(status) {
  if (!status) return;
  setText("vigembus-driver-installed", status.driver_installed ? "Yes" : "No");
  setText("vigembus-driver-running", status.driver_running ? "Yes" : "No");
  setText("vigembus-dll-found", status.dll_found ? "Yes" : "No");
  setText("vigembus-pad-connected", status.virtual_pad_connected ? "Yes" : "No");
  setText("vigembus-xbox-connected", status.xbox_target_connected ? "Yes" : "No");
  setText("vigembus-ds4-connected", status.ds4_target_connected ? "Yes" : "No");

  const chip = el("vigembus-chip");
  const dot = el("vigembus-dot");
  const label = el("vigembus-label");
  const hint = el("vigembus-hint");
  const installBtn = el("btn-install-vigembus");

  // Determine status color:
  //   green  = driver running AND dll found (virtual pad can work)
  //   yellow = driver installed but DLL missing (or driver not running)
  //   red    = driver not installed
  chip.classList.remove("connected", "disconnected");
  if (status.driver_running && status.dll_found) {
    chip.classList.add("connected");
    label.textContent = "Ready";
    if (hint) hint.textContent = "ViGEmBus is active. Virtual XInput output is enabled.";
    if (installBtn) installBtn.style.display = "none";
  } else if (status.driver_installed) {
    label.textContent = "Partial";
    if (hint) {
      if (!status.driver_running) {
        hint.textContent = "ViGEmBus driver is installed but not running. Try rebooting or reinstalling.";
      } else {
        hint.textContent = "ViGEmBus driver is running but ViGEmClient.dll was not found. Reinstall ViGEmBus to fix.";
      }
    }
    if (installBtn) installBtn.style.display = "";
  } else {
    chip.classList.add("disconnected");
    label.textContent = "Not Installed";
    if (hint) hint.textContent = "ViGEmBus driver is not installed. Click \"Install ViGEmBus\" to download and run the official installer (UAC prompt will appear).";
    if (installBtn) installBtn.style.display = "";
  }
}

async function refreshVigemBusStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("get_vigembus_status");
    updateVigemBusStatus(status);
  } catch (e) {
    appendLog("[ERR] get_vigembus_status failed: " + e, "warn-line");
  }
}

if (el("btn-install-vigembus")) {
  el("btn-install-vigembus").addEventListener("click", async () => {
    if (!invoke) return;
    const btn = el("btn-install-vigembus");
    btn.disabled = true;
    btn.textContent = "Downloading…";
    appendLog("[ViGEmBus] Starting download + installer launch…");
    try {
      await invoke("install_vigembus");
      appendLog("[ViGEmBus] Installer launched — please complete the UAC prompt.");
      btn.textContent = "Installer Launched";
    } catch (e) {
      appendLog("[ERR] install_vigembus failed: " + e, "warn-line");
      btn.textContent = "Install ViGEmBus";
    } finally {
      btn.disabled = false;
    }
  });
}

// ===========================================================================
// Settings tab
// ===========================================================================

// --- Settings sub-navigation (sidebar) ---
document.querySelectorAll(".settings-nav-btn").forEach((btn) => {
  btn.addEventListener("click", () => {
    const target = btn.dataset.settingsSection;
    document.querySelectorAll(".settings-nav-btn").forEach((b) => b.classList.remove("active"));
    btn.classList.add("active");
    document.querySelectorAll(".settings-section").forEach((s) => {
      s.hidden = s.id !== "settings-" + target;
    });
  });
});

// --- Config persistence controls ---
function updateSettingsUI(cfg) {
  const set = (id, val) => { const e = el(id); if (e) e.checked = val; };
  const setVal = (id, val) => { const e = el(id); if (e) e.value = val; };
  const setText = (id, val) => { const e = el(id); if (e) e.textContent = val; };

  set("cfg-persistence-enabled", cfg.config_persistence_enabled);
  set("cfg-hidhide-enabled", cfg.hidhide_enabled);
  set("cfg-hidhide-auto", cfg.hidhide_auto_hide);
  set("cfg-auto-reconnect", cfg.auto_reconnect);
  set("cfg-bt-detection", cfg.bt_power_detection_enabled);
  set("cfg-auto-start", cfg.auto_start ?? false);
  set("cfg-close-to-tray", cfg.tray_minimize ?? cfg.close_to_tray ?? true);
  setVal("cfg-reconnect-interval", cfg.reconnect_interval_s);
  setText("reconnect-interval-val", cfg.reconnect_interval_s);
  setVal("cfg-battery-polling", String(cfg.battery_polling_interval_s));

  const vc = cfg.default_virtual_controller || "xbox360";
  const vcXbox = el("vc-xbox360");
  const vcDs4 = el("vc-dualshock4");
  if (vcXbox) vcXbox.checked = vc === "xbox360";
  if (vcDs4) vcDs4.checked = vc === "dualshock4";
  // Notification config
  if (cfg.notification_config) {
    set("cfg-notif-enabled", cfg.notification_config.enabled);
    set("cfg-notif-critical", cfg.notification_config.critical_enabled);
    set("cfg-notif-warning", cfg.notification_config.warning_enabled);
    set("cfg-notif-info", cfg.notification_config.info_enabled);
    set("cfg-notif-disconnect", cfg.notification_config.notify_disconnect);
    set("cfg-notif-bt-power", cfg.notification_config.notify_bt_power);
    set("cfg-notif-low-battery", cfg.notification_config.notify_low_battery);
    set("cfg-notif-drift", cfg.notification_config.notify_drift);
    set("cfg-notif-reconnect", cfg.notification_config.notify_reconnect);
  }
}

// Load config file path on startup
async function loadConfigPath() {
  if (!invoke) return;
  try {
    const path = await invoke("get_config_file_path");
    const e = el("cfg-file-path");
    if (e) e.textContent = path;
  } catch (_) { /* ignore */ }
}

// Persistence toggle
if (el("cfg-persistence-enabled")) {
  el("cfg-persistence-enabled").addEventListener("change", async (e) => {
    currentConfig.config_persistence_enabled = e.target.checked;
    await pushConfig(currentConfig);
  });
}

// Auto-reconnect toggle
if (el("cfg-auto-reconnect")) {
  el("cfg-auto-reconnect").addEventListener("change", async (e) => {
    currentConfig.auto_reconnect = e.target.checked;
    await pushConfig(currentConfig);
  });
}

// BT detection toggle
if (el("cfg-bt-detection")) {
  el("cfg-bt-detection").addEventListener("change", async (e) => {
    currentConfig.bt_power_detection_enabled = e.target.checked;
    await pushConfig(currentConfig);
  });
}

// Start-with-Windows toggle
if (el("cfg-auto-start")) {
  el("cfg-auto-start").addEventListener("change", async (e) => {
    if (!invoke) return;
    try {
      await invoke("set_auto_start", { enabled: e.target.checked });
      currentConfig.auto_start = e.target.checked;
      await pushConfig(currentConfig);
    } catch (err) {
      appendLog("[ERR] set_auto_start failed: " + err, "warn-line");
    }
  });
}

// Close-to-tray toggle
if (el("cfg-close-to-tray")) {
  el("cfg-close-to-tray").addEventListener("change", async (e) => {
    currentConfig.close_to_tray = e.target.checked;
    currentConfig.tray_minimize = e.target.checked;
    await pushConfig(currentConfig);
  });
}

// Reconnect interval slider
if (el("cfg-reconnect-interval")) {
  el("cfg-reconnect-interval").addEventListener("input", (e) => {
    el("reconnect-interval-val").textContent = e.target.value;
  });
  el("cfg-reconnect-interval").addEventListener("change", async (e) => {
    currentConfig.reconnect_interval_s = parseInt(e.target.value, 10);
    await pushConfig(currentConfig);
  });
}

// Battery polling dropdown
if (el("cfg-battery-polling")) {
  el("cfg-battery-polling").addEventListener("change", async (e) => {
    currentConfig.battery_polling_interval_s = parseInt(e.target.value, 10);
    await pushConfig(currentConfig);
  });
}

// Virtual controller type radio buttons
document.querySelectorAll('input[name="virtual-controller"]').forEach((radio) => {
  radio.addEventListener("change", async (e) => {
    if (!e.target.checked || !invoke) return;
    const kind = e.target.value;
    currentConfig.default_virtual_controller = kind;
    try {
      await invoke("set_virtual_controller_type", { kind });
      appendLog(`[Settings] Virtual controller set to ${kind}`, "hid-line");
    } catch (err) {
      appendLog("[ERR] set_virtual_controller_type failed: " + err, "warn-line");
    }
  });
});

// --- HidHide controls ---
function updateHidHideUI(status) {
  if (!status) return;
  const installedEl = el("hidhide-installed");
  const hiddenEl = el("hidhide-hidden");
  const pathEl = el("hidhide-device-path");
  const msgEl = el("hidhide-message");

  if (installedEl) installedEl.textContent = status.installed ? "Yes" : "No";
  if (hiddenEl) hiddenEl.textContent = status.hidden ? "Hidden" : "Not hidden";
  if (pathEl) pathEl.textContent = status.device_path || "—";
  if (msgEl) msgEl.textContent = status.message || "";

  const toggle = el("cfg-hidhide-enabled");
  if (toggle && status.enabled !== undefined) {
    // Only update the toggle if the user is not currently dragging it to avoid fighting.
    if (document.activeElement !== toggle) {
      toggle.checked = status.enabled;
    }
  }
}

async function refreshHidHideStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("hidhide_get_status");
    updateHidHideUI(status);
  } catch (e) {
    appendLog("[ERR] hidhide_get_status failed: " + e, "warn-line");
  }
}

if (el("cfg-hidhide-enabled")) {
  el("cfg-hidhide-enabled").addEventListener("change", async (e) => {
    if (!invoke) return;
    const enabled = e.target.checked;
    try {
      const status = await invoke("hidhide_set_enabled", { enabled });
      currentConfig.hidhide_enabled = enabled;
      updateHidHideUI(status);
      await pushConfig(currentConfig);
    } catch (err) {
      appendLog("[ERR] hidhide_set_enabled failed: " + err, "warn-line");
      e.target.checked = !enabled;
    }
  });
}

if (el("cfg-hidhide-auto")) {
  el("cfg-hidhide-auto").addEventListener("change", async (e) => {
    currentConfig.hidhide_auto_hide = e.target.checked;
    await pushConfig(currentConfig);
  });
}

if (el("btn-refresh-hidhide")) {
  el("btn-refresh-hidhide").addEventListener("click", () => {
    refreshHidHideStatus();
    appendLog("[HidHide] Status refreshed");
  });
}

// --- Notification toggles ---
function ensureNotifConfig() {
  if (!currentConfig.notification_config) {
    currentConfig.notification_config = {
      enabled: true,
      critical_enabled: true,
      warning_enabled: true,
      info_enabled: true,
      notify_disconnect: true,
      notify_bt_power: true,
      notify_low_battery: true,
      notify_drift: true,
      notify_reconnect: true,
    };
  }
  return currentConfig.notification_config;
}

// Per-event toggle handlers
[
  ["cfg-notif-disconnect", "notify_disconnect"],
  ["cfg-notif-bt-power", "notify_bt_power"],
  ["cfg-notif-low-battery", "notify_low_battery"],
  ["cfg-notif-drift", "notify_drift"],
  ["cfg-notif-reconnect", "notify_reconnect"],
].forEach(([id, field]) => {
  if (el(id)) {
    el(id).addEventListener("change", async (e) => {
      const nc = ensureNotifConfig();
      nc[field] = e.target.checked;
      await pushConfig(currentConfig);
    });
  }
});

// Category toggle → cascades to all per-event toggles inside its accordion
const categoryCascade = {
  "cfg-notif-critical": {
    field: "critical_enabled",
    events: ["cfg-notif-disconnect", "cfg-notif-bt-power"],
    eventFields: ["notify_disconnect", "notify_bt_power"],
  },
  "cfg-notif-warning": {
    field: "warning_enabled",
    events: ["cfg-notif-low-battery", "cfg-notif-drift"],
    eventFields: ["notify_low_battery", "notify_drift"],
  },
  "cfg-notif-info": {
    field: "info_enabled",
    events: ["cfg-notif-reconnect"],
    eventFields: ["notify_reconnect"],
  },
};

Object.entries(categoryCascade).forEach(([catId, { field, events, eventFields }]) => {
  if (el(catId)) {
    el(catId).addEventListener("change", async (e) => {
      const nc = ensureNotifConfig();
      nc[field] = e.target.checked;
      // Cascade to all per-event toggles in this category
      events.forEach((evtId, i) => {
        nc[eventFields[i]] = e.target.checked;
        const evtEl = el(evtId);
        if (evtEl) evtEl.checked = e.target.checked;
      });
      await pushConfig(currentConfig);
    });
  }
});

// Master toggle → cascades to all category + per-event toggles
if (el("cfg-notif-enabled")) {
  el("cfg-notif-enabled").addEventListener("change", async (e) => {
    const nc = ensureNotifConfig();
    nc.enabled = e.target.checked;
    // Cascade to all category toggles
    Object.entries(categoryCascade).forEach(([catId, { field, events, eventFields }]) => {
      nc[field] = e.target.checked;
      const catEl = el(catId);
      if (catEl) catEl.checked = e.target.checked;
      // And their per-event toggles
      events.forEach((evtId, i) => {
        nc[eventFields[i]] = e.target.checked;
        const evtEl = el(evtId);
        if (evtEl) evtEl.checked = e.target.checked;
      });
    });
    await pushConfig(currentConfig);
  });
}

// --- Notification accordion expand/collapse ---
document.querySelectorAll(".accordion-header").forEach((header) => {
  // Click on the header (but not on the toggle) toggles the accordion
  header.addEventListener("click", (e) => {
    // Don't toggle if the click was on the toggle switch or its label
    if (e.target.closest(".toggle-switch") || e.target.closest(".accordion-label")) {
      // But allow click on the chevron or empty header area
      if (!e.target.closest(".accordion-chevron") && !e.target.classList.contains("accordion-header")) {
        return;
      }
    }
    const accordion = header.closest(".accordion");
    const body = accordion.querySelector(".accordion-body");
    const isExpanded = !body.hidden;
    body.hidden = isExpanded;
    accordion.classList.toggle("expanded", !isExpanded);
    header.setAttribute("aria-expanded", String(!isExpanded));
  });
  // Keyboard support: Enter/Space toggles
  header.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      header.click();
    }
  });
});

// --- Export config ---
if (el("btn-export-config")) {
  el("btn-export-config").addEventListener("click", async () => {
    if (!invoke) return;
    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const path = await save({
        defaultPath: "oxidelink-config.json",
        filters: [{ name: "JSON", extensions: ["json"] }],
      });
      if (!path) return;
      await invoke("export_config_to_file", { path });
      appendLog("[Settings] Config exported to " + path);
    } catch (e) {
      appendLog("[ERR] Export failed: " + e, "warn-line");
    }
  });
}

// --- Import config ---
if (el("btn-import-config")) {
  el("btn-import-config").addEventListener("click", async () => {
    if (!invoke) return;
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const path = await open({
        filters: [{ name: "JSON", extensions: ["json"] }],
        multiple: false,
      });
      if (!path) return;
      const confirmed = await showConfirmDialog(
        "Import Configuration",
        "This will replace ALL current settings with the values from the imported file. Continue?"
      );
      if (!confirmed) return;
      const newConfig = await invoke("import_config_from_file", { path });
      currentConfig = newConfig;
      updateConfig(newConfig);
      updateSettingsUI(newConfig);
      appendLog("[Settings] Config imported from " + path);
    } catch (e) {
      appendLog("[ERR] Import failed: " + e, "warn-line");
    }
  });
}

// --- Reset to defaults ---
if (el("btn-reset-config")) {
  el("btn-reset-config").addEventListener("click", async () => {
    if (!invoke) return;
    const confirmed = await showConfirmDialog(
      "Reset All Settings",
      "This will reset ALL settings to their default values. This cannot be undone. Continue?"
    );
    if (!confirmed) return;
    try {
      const defaults = await invoke("reset_config_to_defaults");
      currentConfig = defaults;
      updateConfig(defaults);
      updateSettingsUI(defaults);
      appendLog("[Settings] All settings reset to defaults");
    } catch (e) {
      appendLog("[ERR] Reset failed: " + e, "warn-line");
    }
  });
}

// --- About section ---
if (el("btn-open-github")) {
  el("btn-open-github").addEventListener("click", async () => {
    try {
      const { open } = await import("@tauri-apps/plugin-shell");
      await open("https://github.com/dekuNukem/Nintendo_Switch_Reverse_Engineering");
    } catch (e) {
      appendLog("[ERR] Failed to open URL: " + e, "warn-line");
    }
  });
}

if (el("btn-open-logs")) {
  el("btn-open-logs").addEventListener("click", async () => {
    try {
      const { open } = await import("@tauri-apps/plugin-shell");
      const path = await invoke("get_config_file_path");
      // Open the directory containing the config file
      const dir = path.replace(/config\.json$/, "");
      await open(dir);
    } catch (e) {
      appendLog("[ERR] Failed to open logs folder: " + e, "warn-line");
    }
  });
}

// --- Confirmation dialog helper ---
function showConfirmDialog(title, body) {
  return new Promise((resolve) => {
    // Remove any existing modal
    const existing = document.querySelector(".modal-overlay");
    if (existing) existing.remove();

    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal" role="alertdialog" aria-modal="true" aria-labelledby="modal-title" aria-describedby="modal-body">
        <h3 class="modal-title" id="modal-title">${escapeHtml(title)}</h3>
        <p class="modal-body" id="modal-body">${escapeHtml(body)}</p>
        <div class="modal-actions">
          <button class="btn" id="modal-cancel">Cancel</button>
          <button class="btn btn-danger" id="modal-confirm">Confirm</button>
        </div>
      </div>
    `;
    document.body.appendChild(overlay);

    const cancel = overlay.querySelector("#modal-cancel");
    const confirm = overlay.querySelector("#modal-confirm");

    const cleanup = (result) => {
      overlay.remove();
      resolve(result);
    };

    cancel.addEventListener("click", () => cleanup(false));
    confirm.addEventListener("click", () => cleanup(true));
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) cleanup(false);
    });
    // Keyboard: Escape = cancel, Enter = confirm
    overlay.addEventListener("keydown", (e) => {
      if (e.key === "Escape") cleanup(false);
      if (e.key === "Enter") cleanup(true);
    });
    // Focus the cancel button by default (safer default)
    cancel.focus();
  });
}

// --- Update About section with device info ---
function updateAboutSection(deviceInfo) {
  if (!deviceInfo) return;
  const fw = el("about-fw-version");
  const mac = el("about-mac");
  if (fw && deviceInfo.firmware_version) fw.textContent = deviceInfo.firmware_version;
  if (mac && deviceInfo.mac_address) mac.textContent = deviceInfo.mac_address;
}

// --- Initialize settings on load ---
loadConfigPath();
// Update settings UI when config is received via IPC
const origUpdateConfig = typeof updateConfig === "function" ? updateConfig : null;
if (origUpdateConfig) {
  const wrappedUpdate = (cfg) => {
    if (cfg && typeof cfg === "object") {
      Object.assign(currentConfig, cfg);
    }
    origUpdateConfig(cfg);
    updateSettingsUI(cfg);
  };
  // Replace the global reference
  window.updateConfig = wrappedUpdate;
}

if (el("btn-refresh-vigembus")) {
  el("btn-refresh-vigembus").addEventListener("click", () => {
    refreshVigemBusStatus();
    appendLog("[ViGEmBus] Status refreshed");
  });
}

// Poll ViGEmBus status on load and every 10s.
refreshVigemBusStatus();
setInterval(refreshVigemBusStatus, 10000);

// =============================================================================
// SPI flash re-read button
// =============================================================================
if (el("btn-refresh-spi")) {
  el("btn-refresh-spi").addEventListener("click", () => {
    if (!invoke) return;
    setText("spi-cal-status", "Reading…");
    setText("spi-serial", "Reading…");
    appendLog("[SPI] Re-reading SPI flash data…");
    invoke("refresh_spi_diagnostics").then(() => {
      appendLog("[SPI] Re-read subcommands sent");
    }).catch((err) => handleError("refresh_spi_diagnostics failed", err));
  });
}

// =============================================================================
// IMU sensitivity configuration
// =============================================================================
async function setImuSensitivity(gyroRange, accelRange, gyroRate, accelFilter) {
  if (!invoke) {
    appendLog("[IMU] Sensitivity requires Tauri invoke (not in browser)", "warn-line");
    return;
  }
  try {
    await invoke("set_imu_sensitivity", {
      gyroRange, accelRange, gyroRate, accelFilter
    });
    appendLog("[IMU] Sensitivity set: gyro=" + gyroRange + " accel=" + accelRange);
  } catch (e) {
    appendLog("[ERR] set_imu_sensitivity failed: " + e, "warn-line");
  }
}

el("btn-apply-imu-sensitivity")?.addEventListener("click", () => {
  const gyro = parseInt(el("imu-gyro-range")?.value ?? "3", 10);
  const accel = parseInt(el("imu-accel-range")?.value ?? "0", 10);
  setImuSensitivity(gyro, accel, 1, 1); // default rate and filter
});

// =============================================================================
// Battery voltage refresh (granular mV from subcommand 0x50)
// =============================================================================
async function refreshBatteryVoltage() {
  if (!invoke) return;
  try {
    const mv = await invoke("get_battery_voltage");
    if (mv > 0) {
      const bvEl = el("battery-voltage");
      if (bvEl) bvEl.textContent = mv + " mV";
    }
  } catch (e) {
    appendLog("[ERR] get_battery_voltage failed: " + e, "warn-line");
  }
}

// Poll battery voltage every 5s when in Tauri
if (invoke) {
  setInterval(refreshBatteryVoltage, 5000);
}

// Poll HidHide status on load and every 10s when in Tauri
if (invoke) {
  refreshHidHideStatus();
  setInterval(refreshHidHideStatus, 10000);
}

// =============================================================================
// Profiles manager
// =============================================================================
let profilesCache = [];
let selectedProfileId = null;
let editingRules = [];

function updateProfileIndicator(id, name) {
  const node = el("active-profile-name");
  if (node) node.textContent = name || id || "—";
}

async function loadProfiles() {
  if (!invoke) return;
  try {
    profilesCache = await invoke("list_profiles");
    renderProfileList();
    const active = await invoke("get_active_profile");
    updateProfileIndicator(active?.id, active?.name);
    const auto = await invoke("get_auto_switch_enabled");
    const t = el("auto-switch-toggle");
    if (t) t.checked = auto;
  } catch (e) {
    appendLog("[ERR] loadProfiles: " + e, "warn-line");
  }
}

function renderProfileList() {
  const list = el("profile-list");
  if (!list) return;
  list.innerHTML = "";
  profilesCache.forEach((p) => {
    const opt = document.createElement("option");
    opt.value = p.id;
    opt.textContent = (p.enabled ? "" : "[OFF] ") + p.name;
    if (p.id === selectedProfileId) opt.selected = true;
    list.appendChild(opt);
  });
}

function renderProfileEditor(profile) {
  const nameInput = el("profile-name");
  const enabledBox = el("profile-enabled");
  if (!nameInput) return;

  if (profile) {
    nameInput.value = profile.name || "";
    enabledBox.checked = profile.enabled !== false;
    editingRules = (profile.auto_rules || []).map((r) => ({ ...r }));
  } else {
    nameInput.value = "";
    enabledBox.checked = true;
    editingRules = [];
  }
  renderRules();
}

function renderRules() {
  const container = el("profile-rules-list");
  if (!container) return;
  container.innerHTML = "";
  editingRules.forEach((rule, idx) => {
    const row = document.createElement("div");
    row.className = "rule-row";
    row.style.cssText = "display:flex;gap:0.5rem;align-items:center;margin-bottom:0.4rem;flex-wrap:wrap";
    row.innerHTML = `
      <select class="rule-kind" data-idx="${idx}" style="min-width:110px">
        <option value="process_path" ${rule.kind === "process_path" ? "selected" : ""}>Process Path</option>
        <option value="window_title" ${rule.kind === "window_title" ? "selected" : ""}>Window Title</option>
      </select>
      <select class="rule-mode" data-idx="${idx}" style="min-width:90px">
        <option value="exact" ${rule.match_mode === "exact" ? "selected" : ""}>Exact</option>
        <option value="contains" ${rule.match_mode === "contains" ? "selected" : ""}>Contains</option>
        <option value="regex" ${rule.match_mode === "regex" ? "selected" : ""}>Regex</option>
      </select>
      <input type="text" class="rule-pattern" data-idx="${idx}" value="${escapeHtml(rule.pattern || "")}" placeholder="Pattern" style="flex:1;min-width:120px" />
      <label><input type="checkbox" class="rule-enabled" data-idx="${idx}" ${rule.enabled ? "checked" : ""} /> On</label>
      <button class="btn btn-remove-rule" data-idx="${idx}" type="button">×</button>
    `;
    container.appendChild(row);
  });

  container.querySelectorAll(".rule-kind").forEach((s) =>
    s.addEventListener("change", (e) => {
      editingRules[e.target.dataset.idx].kind = e.target.value;
    })
  );
  container.querySelectorAll(".rule-mode").forEach((s) =>
    s.addEventListener("change", (e) => {
      editingRules[e.target.dataset.idx].match_mode = e.target.value;
    })
  );
  container.querySelectorAll(".rule-pattern").forEach((i) =>
    i.addEventListener("input", (e) => {
      editingRules[e.target.dataset.idx].pattern = e.target.value;
    })
  );
  container.querySelectorAll(".rule-enabled").forEach((c) =>
    c.addEventListener("change", (e) => {
      editingRules[e.target.dataset.idx].enabled = e.target.checked;
    })
  );
  container.querySelectorAll(".btn-remove-rule").forEach((b) =>
    b.addEventListener("click", (e) => {
      editingRules.splice(e.target.dataset.idx, 1);
      renderRules();
    })
  );
}

function getSelectedProfile() {
  const list = el("profile-list");
  if (!list || list.selectedIndex < 0) return null;
  const id = list.value;
  return profilesCache.find((p) => p.id === id) || null;
}

async function saveProfile() {
  if (!invoke) return;
  const name = el("profile-name").value.trim();
  if (!name) return appendLog("[ERR] Profile name required", "warn-line");
  const enabled = el("profile-enabled").checked;
  const profile = getSelectedProfile();
  try {
    if (profile) {
      const updated = { ...profile, name, enabled, auto_rules: editingRules, updated_at: Date.now() };
      await invoke("update_profile", { profile: updated });
      appendLog("[PROF] Profile updated", "hid-line");
    } else {
      await invoke("create_profile", { name, auto_rules: editingRules });
      appendLog("[PROF] Profile created", "hid-line");
    }
    await loadProfiles();
  } catch (e) {
    appendLog("[ERR] saveProfile: " + e, "warn-line");
  }
}

async function deleteProfile() {
  if (!invoke) return;
  const profile = getSelectedProfile();
  if (!profile) return;
  try {
    await invoke("delete_profile", { id: profile.id });
    selectedProfileId = null;
    renderProfileEditor(null);
    await loadProfiles();
    appendLog("[PROF] Profile deleted", "hid-line");
  } catch (e) {
    appendLog("[ERR] deleteProfile: " + e, "warn-line");
  }
}

async function setActiveFromSelection() {
  if (!invoke) return;
  const profile = getSelectedProfile();
  try {
    await invoke("set_active_profile", { id: profile ? profile.id : null });
    appendLog("[PROF] Active profile set", "hid-line");
  } catch (e) {
    appendLog("[ERR] setActiveFromSelection: " + e, "warn-line");
  }
}

async function toggleAutoSwitch() {
  if (!invoke) return;
  const enabled = el("auto-switch-toggle").checked;
  try {
    await invoke("set_auto_switch_enabled", { enabled });
    appendLog("[PROF] Auto-switch " + (enabled ? "enabled" : "disabled"), "hid-line");
  } catch (e) {
    appendLog("[ERR] toggleAutoSwitch: " + e, "warn-line");
  }
}

async function exportProfiles() {
  if (!invoke) return;
  const path = prompt("Enter full export file path:", "C:\\\\Users\\\\%USERNAME%\\\\Documents\\\\OxideLink-profiles.json");
  if (!path) return;
  try {
    const saved = await invoke("export_profiles", { path });
    appendLog("[PROF] Exported to " + saved, "hid-line");
  } catch (e) {
    appendLog("[ERR] exportProfiles: " + e, "warn-line");
  }
}

async function importProfiles() {
  if (!invoke) return;
  const path = prompt("Enter full import file path:");
  if (!path) return;
  try {
    const list = await invoke("import_profiles", { path });
    profilesCache = list;
    renderProfileList();
    appendLog("[PROF] Imported " + list.length + " profiles", "hid-line");
  } catch (e) {
    appendLog("[ERR] importProfiles: " + e, "warn-line");
  }
}

el("profile-list")?.addEventListener("change", () => {
  const profile = getSelectedProfile();
  selectedProfileId = profile?.id || null;
  renderProfileEditor(profile);
});

el("btn-new-profile")?.addEventListener("click", () => {
  selectedProfileId = null;
  el("profile-list").selectedIndex = -1;
  renderProfileEditor(null);
});

el("btn-save-profile")?.addEventListener("click", saveProfile);
el("btn-cancel-profile")?.addEventListener("click", () => {
  renderProfileEditor(getSelectedProfile());
});
el("btn-delete-profile")?.addEventListener("click", deleteProfile);
el("btn-set-active")?.addEventListener("click", setActiveFromSelection);
el("auto-switch-toggle")?.addEventListener("change", toggleAutoSwitch);
el("btn-add-rule")?.addEventListener("click", () => {
  editingRules.push({
    kind: "process_path",
    pattern: "",
    match_mode: "contains",
    enabled: true,
  });
  renderRules();
});
el("btn-export-profiles")?.addEventListener("click", exportProfiles);
el("btn-import-profiles")?.addEventListener("click", importProfiles);

// =============================================================================
// Macros engine
// =============================================================================
let macrosCache = [];
let selectedMacroId = null;
let macroSteps = [];

function updateMacroStepFields() {
  const type = el("macro-step-type")?.value || "wait_ms";
  el("macro-step-fields")?.querySelectorAll("[data-for]").forEach((node) => {
    const modes = (node.dataset.for || "").split(" ");
    const show = modes.includes(type);
    node.style.display = show ? "" : "none";
  });
}

function buildMacroStep() {
  const type = el("macro-step-type")?.value;
  if (!type) return null;
  switch (type) {
    case "wait_ms":
      return { type: "wait_ms", value: parseInt(el("macro-wait")?.value || "0", 10) };
    case "press_button":
    case "release_button":
      return { type, value: el("macro-btn")?.value || "a" };
    case "key_down":
    case "key_up":
      return { type, value: el("macro-key")?.value || "a" };
    case "mouse_move":
      return {
        type: "mouse_move",
        value: [
          parseInt(el("macro-mouse-x")?.value || "0", 10),
          parseInt(el("macro-mouse-y")?.value || "0", 10),
        ],
      };
    case "mouse_down":
    case "mouse_up":
      return { type, value: parseInt(el("macro-mouse-btn")?.value || "0", 10) };
    case "set_stick":
      return {
        type: "set_stick",
        value: [
          el("macro-stick-side")?.value || "left",
          parseFloat(el("macro-stick-x")?.value || "0"),
          parseFloat(el("macro-stick-y")?.value || "0"),
        ],
      };
    case "set_trigger":
      return {
        type: "set_trigger",
        value: [
          el("macro-trigger-side")?.value || "left",
          parseFloat(el("macro-trigger-value")?.value || "0"),
        ],
      };
  }
  return null;
}

function renderMacroSteps() {
  const list = el("macro-steps");
  if (!list) return;
  list.innerHTML = "";
  macroSteps.forEach((s, idx) => {
    const li = document.createElement("li");
    li.textContent = JSON.stringify(s);
    li.title = "Click to remove";
    li.style.cursor = "pointer";
    li.addEventListener("click", () => {
      macroSteps.splice(idx, 1);
      renderMacroSteps();
    });
    list.appendChild(li);
  });
}

function updateMacroStatus(text) {
  const node = el("macro-status");
  if (node) node.textContent = text;
}

async function loadMacros() {
  if (!invoke) return;
  try {
    macrosCache = await invoke("macro_list") || [];
    renderMacroList();
  } catch (e) {
    appendLog("[ERR] macro_list: " + e, "warn-line");
  }
}

function renderMacroList() {
  const list = el("macro-list");
  if (!list) return;
  list.innerHTML = "";
  macrosCache.forEach((m) => {
    const li = document.createElement("li");
    li.textContent = m.name || m.id;
    li.dataset.id = m.id;
    li.style.cursor = "pointer";
    if (m.id === selectedMacroId) li.classList.add("selected");
    li.addEventListener("click", () => selectMacro(m.id));
    list.appendChild(li);
  });
  const play = el("btn-macro-play");
  const del = el("btn-macro-delete");
  if (play) play.disabled = !selectedMacroId;
  if (del) del.disabled = !selectedMacroId;
}

async function selectMacro(id) {
  selectedMacroId = id;
  if (!invoke || !id) {
    el("macro-name").value = "";
    macroSteps = [];
    renderMacroSteps();
    renderMacroList();
    return;
  }
  try {
    const mac = await invoke("macro_load", { id });
    if (mac) {
      el("macro-name").value = mac.name || "";
      macroSteps = Array.isArray(mac.steps) ? mac.steps : [];
    } else {
      el("macro-name").value = "";
      macroSteps = [];
    }
    renderMacroSteps();
    renderMacroList();
  } catch (e) {
    appendLog("[ERR] macro_load: " + e, "warn-line");
  }
}

async function saveMacro() {
  if (!invoke) return;
  const name = el("macro-name")?.value?.trim();
  if (!name) return appendLog("[ERR] Macro name required", "warn-line");
  const mac = { id: selectedMacroId || "", name, steps: macroSteps };
  try {
    if (selectedMacroId) {
      await invoke("macro_update", { mac });
      updateMacroStatus("Updated");
    } else {
      await invoke("macro_create", { mac });
      updateMacroStatus("Created");
    }
    await loadMacros();
  } catch (e) {
    updateMacroStatus("Save failed: " + e);
    appendLog("[ERR] saveMacro: " + e, "warn-line");
  }
}

async function deleteMacro() {
  if (!invoke || !selectedMacroId) return;
  try {
    await invoke("macro_delete", { id: selectedMacroId });
    selectedMacroId = null;
    macroSteps = [];
    el("macro-name").value = "";
    renderMacroSteps();
    await loadMacros();
  } catch (e) {
    appendLog("[ERR] macro_delete: " + e, "warn-line");
  }
}

async function playMacro() {
  if (!invoke || !selectedMacroId) return;
  try {
    await invoke("macro_play", { id: selectedMacroId });
    updateMacroStatus("Playing " + selectedMacroId);
  } catch (e) {
    updateMacroStatus("Play failed: " + e);
  }
}

async function stopMacro() {
  if (!invoke) return;
  try {
    await invoke("macro_stop");
    updateMacroStatus("Stopped");
  } catch (e) {
    appendLog("[ERR] macro_stop: " + e, "warn-line");
  }
}

async function recordStart() {
  if (!invoke) return;
  try {
    await invoke("macro_record_start");
    updateMacroStatus("Recording...");
    el("btn-macro-record").disabled = true;
    el("btn-macro-stop").disabled = false;
  } catch (e) {
    updateMacroStatus("Record failed: " + e);
  }
}

async function recordStop() {
  if (!invoke) return;
  const name = el("macro-name")?.value?.trim() || ("recording-" + Date.now());
  try {
    const mac = await invoke("macro_record_stop", { name });
    if (mac) {
      el("macro-name").value = mac.name || "";
      macroSteps = Array.isArray(mac.steps) ? mac.steps : [];
      selectedMacroId = mac.id || null;
      renderMacroSteps();
      await loadMacros();
    }
    el("btn-macro-record").disabled = false;
    el("btn-macro-stop").disabled = true;
    updateMacroStatus("Recorded");
  } catch (e) {
    updateMacroStatus("Record stop failed: " + e);
    el("btn-macro-record").disabled = false;
    el("btn-macro-stop").disabled = true;
  }
}

el("macro-step-type")?.addEventListener("change", updateMacroStepFields);
updateMacroStepFields();
el("btn-macro-add-step")?.addEventListener("click", () => {
  const step = buildMacroStep();
  if (step) {
    macroSteps.push(step);
    renderMacroSteps();
  }
});
el("btn-macro-save")?.addEventListener("click", saveMacro);
el("btn-macro-delete")?.addEventListener("click", deleteMacro);
el("btn-macro-play")?.addEventListener("click", playMacro);
el("btn-macro-stop")?.addEventListener("click", stopMacro);
el("btn-macro-record")?.addEventListener("click", recordStart);
el("btn-macro-stop")?.addEventListener("click", recordStop);

// =============================================================================
// Response curves / stick zones
// =============================================================================
function showCurveFields() {
  const type = el("curve-type")?.value;
  document.querySelectorAll("[data-curve]").forEach((node) => {
    node.style.display = (node.dataset.curve === type || (type === "bezier" && node.dataset.curve === "bezier")) ? "" : "none";
  });
  document.querySelectorAll("[data-curve='exponential']").forEach((node) => {
    node.style.display = (type === "exponential") ? "" : "none";
  });
  document.querySelectorAll("[data-curve='bezier']").forEach((node) => {
    node.style.display = (type === "bezier") ? "" : "none";
  });
}

async function loadCurve() {
  if (!invoke) return;
  try {
    const curve = await invoke("get_response_curve");
    if (!curve) return;
    el("curve-type").value = curve.type;
    if (curve.type === "exponential") el("curve-power").value = curve.value;
    if (curve.type === "bezier") {
      el("curve-p1x").value = curve.value.p1[0];
      el("curve-p1y").value = curve.value.p1[1];
      el("curve-p2x").value = curve.value.p2[0];
      el("curve-p2y").value = curve.value.p2[1];
    }
    showCurveFields();
  } catch (e) {
    appendLog("[ERR] get_response_curve: " + e, "warn-line");
  }
}

async function applyCurve() {
  if (!invoke) return;
  const type = el("curve-type")?.value;
  let curve;
  if (type === "linear" || type === "scurve") {
    curve = { type };
  } else if (type === "exponential") {
    curve = { type: "exponential", value: parseFloat(el("curve-power")?.value || "2") };
  } else if (type === "bezier") {
    curve = {
      type: "bezier",
      value: {
        p1: [parseFloat(el("curve-p1x")?.value || "0.3"), parseFloat(el("curve-p1y")?.value || "0.9")],
        p2: [parseFloat(el("curve-p2x")?.value || "0.7"), parseFloat(el("curve-p2y")?.value || "0.1")],
      },
    };
  } else {
    return;
  }
  try {
    await invoke("set_mapping_response_curve", { curve });
    el("curve-status").textContent = "Curve applied";
  } catch (e) {
    el("curve-status").textContent = "Error: " + e;
  }
}

async function loadZones() {
  if (!invoke) return;
  try {
    const zones = await invoke("get_stick_zones");
    el("zone-deadzone").value = zones.deadzone;
    el("zone-low").value = zones.low;
    el("zone-medium").value = zones.medium;
    el("zone-high").value = zones.high;
  } catch (e) {
    appendLog("[ERR] get_stick_zones: " + e, "warn-line");
  }
}

async function applyZones() {
  if (!invoke) return;
  const zones = {
    deadzone: parseFloat(el("zone-deadzone")?.value || "0"),
    low: parseFloat(el("zone-low")?.value || "0.25"),
    medium: parseFloat(el("zone-medium")?.value || "0.5"),
    high: parseFloat(el("zone-high")?.value || "0.75"),
    low_actions: [],
    medium_actions: [],
    high_actions: [],
  };
  try {
    await invoke("set_stick_zones", { zones });
  } catch (e) {
    appendLog("[ERR] set_stick_zones: " + e, "warn-line");
  }
}

el("curve-type")?.addEventListener("change", showCurveFields);
el("btn-curve-load")?.addEventListener("click", loadCurve);
el("btn-curve-apply")?.addEventListener("click", applyCurve);
el("btn-zone-load")?.addEventListener("click", loadZones);
el("btn-zone-apply")?.addEventListener("click", applyZones);
showCurveFields();

// =============================================================================
// KB/M
// =============================================================================
async function loadKbm() {
  if (!invoke) return;
  try {
    const cfg = await invoke("kbm_get_status");
    el("kbm-enabled").checked = !!cfg.enabled;
    el("kbm-anti-cheat").checked = !!cfg.anti_cheat_mode;
    el("kbm-mouse-sens").value = cfg.mouse_sensitivity;
    el("kbm-key-delay").value = cfg.key_repeat_delay_ms;
    el("kbm-key-rate").value = cfg.key_repeat_rate_ms;
    el("kbm-status").textContent = cfg.enabled ? "Enabled" : "Disabled";
  } catch (e) {
    appendLog("[ERR] kbm_get_status: " + e, "warn-line");
  }
}

async function applyKbm() {
  if (!invoke) return;
  const enabled = el("kbm-enabled")?.checked || false;
  try {
    await invoke("kbm_set_enabled", { enabled });
    if (currentConfig) {
      currentConfig.kbm_config = currentConfig.kbm_config || {};
      currentConfig.kbm_config.enabled = enabled;
      currentConfig.kbm_config.anti_cheat_mode = el("kbm-anti-cheat")?.checked || false;
      currentConfig.kbm_config.mouse_sensitivity = parseFloat(el("kbm-mouse-sens")?.value || "1");
      currentConfig.kbm_config.key_repeat_delay_ms = parseInt(el("kbm-key-delay")?.value || "250", 10);
      currentConfig.kbm_config.key_repeat_rate_ms = parseInt(el("kbm-key-rate")?.value || "33", 10);
      await invoke("update_config", { config: currentConfig });
    }
    await loadKbm();
  } catch (e) {
    appendLog("[ERR] applyKbm: " + e, "warn-line");
  }
}

async function testKbmKey(down) {
  if (!invoke) return;
  const key = el("kbm-test-key")?.value?.trim() || "a";
  try {
    await invoke("kbm_send_test_key", { key, down });
  } catch (e) {
    appendLog("[ERR] kbm_send_test_key: " + e, "warn-line");
  }
}

el("btn-kbm-apply")?.addEventListener("click", applyKbm);
el("btn-kbm-test-down")?.addEventListener("click", () => testKbmKey(true));
el("btn-kbm-test-up")?.addEventListener("click", () => testKbmKey(false));

// =============================================================================
// HidHide tab
// =============================================================================
function updateHidHideTabUI(status) {
  if (!status) return;
  setText("hidhide-tab-installed", status.installed ? "Yes" : "No");
  setText("hidhide-tab-hidden", status.hidden ? "Yes" : "No");
  setText("hidhide-tab-path", status.device_path || "—");
  setText("hidhide-tab-message", status.message || "");
  const t = el("hidhide-tab-enabled");
  if (t && status.enabled !== undefined && document.activeElement !== t) t.checked = status.enabled;
}

async function refreshHidHideTabStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("hidhide_get_status");
    updateHidHideTabUI(status);
  } catch (e) {
    appendLog("[ERR] hidhide_get_status: " + e, "warn-line");
  }
}

async function setHidHideTabEnabled(enabled) {
  if (!invoke) return;
  try {
    const status = await invoke("hidhide_set_enabled", { enabled });
    updateHidHideTabUI(status);
    if (currentConfig) {
      currentConfig.hidhide_enabled = enabled;
      await pushConfig(currentConfig);
    }
  } catch (e) {
    appendLog("[ERR] hidhide_set_enabled: " + e, "warn-line");
    el("hidhide-tab-enabled").checked = !enabled;
  }
}

el("btn-hidhide-refresh")?.addEventListener("click", refreshHidHideTabStatus);
el("btn-hidhide-hide")?.addEventListener("click", () => setHidHideTabEnabled(true));
el("btn-hidhide-unhide")?.addEventListener("click", () => setHidHideTabEnabled(false));
el("hidhide-tab-enabled")?.addEventListener("change", (e) => setHidHideTabEnabled(e.target.checked));

// =============================================================================
// Logging tab (reuses appLogs)
// =============================================================================
let loggingLiveTail = true;

function renderLoggingLogs() {
  const body = el("logging-body");
  if (!body) return;
  const level = (el("logging-level-filter")?.value || "").toLowerCase();
  const query = (el("logging-search")?.value || "").toLowerCase();
  const frag = document.createDocumentFragment();
  let count = 0;
  const maxRows = 200;
  for (let i = appLogs.length - 1; i >= 0; i--) {
    const e = appLogs[i];
    if (level && e.level !== level) continue;
    if (query && !(`${e.target || ""} ${e.message || ""}`.toLowerCase().includes(query))) continue;
    const tr = document.createElement("tr");
    tr.className = logLevelClass(e.level);
    tr.innerHTML = `<td>${escapeHtml(formatLogTimestamp(e.timestamp))}</td><td>${escapeHtml(e.level)}</td><td>${escapeHtml(e.target)}</td><td>${escapeHtml(e.message)}</td>`;
    frag.appendChild(tr);
    if (++count >= maxRows) break;
  }
  body.innerHTML = "";
  // Reverse to chronological order
  const arr = Array.from(frag.childNodes).reverse();
  arr.forEach((n) => body.appendChild(n));
}

async function refreshLoggingLogs() {
  if (!invoke) return;
  try {
    const level = el("logging-level-filter")?.value || null;
    const search = el("logging-search")?.value || null;
    const logs = await invoke("get_logs", { level, search, limit: 1000 });
    appLogs = logs || [];
    renderLoggingLogs();
    renderAppLogs();
  } catch (e) {
    appendLog("[ERR] get_logs: " + e, "warn-line");
  }
}

function copyLoggingLogs() {
  const text = appLogs
    .map((e) => `${formatLogTimestamp(e.timestamp)} [${e.level}] ${e.target}: ${e.message}`)
    .join("\n");
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).catch((err) =>
      handleError("copy to clipboard failed", err)
    );
  }
}

async function clearLoggingLogs() {
  if (!invoke) return;
  try {
    await invoke("clear_logs");
    appLogs = [];
    renderLoggingLogs();
    renderAppLogs();
  } catch (e) {
    appendLog("[ERR] clear_logs: " + e, "warn-line");
  }
}

async function setLoggingLevel() {
  if (!invoke) return;
  const level = el("logging-set-level")?.value;
  if (!level) return;
  try {
    await invoke("set_log_level", { level });
    appendLog("[LOG] Level set to " + level, "hid-line");
  } catch (e) {
    appendLog("[ERR] set_log_level: " + e, "warn-line");
  }
}

el("logging-level-filter")?.addEventListener("change", renderLoggingLogs);
el("logging-search")?.addEventListener("input", renderLoggingLogs);
el("logging-live")?.addEventListener("change", (e) => { loggingLiveTail = e.target.checked; });
el("btn-logging-refresh")?.addEventListener("click", refreshLoggingLogs);
el("btn-logging-copy")?.addEventListener("click", copyLoggingLogs);
el("btn-logging-clear")?.addEventListener("click", clearLoggingLogs);
el("logging-set-level")?.addEventListener("change", setLoggingLevel);

// =============================================================================
// Tray / Startup tab
// =============================================================================
function updateTrayTabUI(state) {
  if (!state) return;
  setText("tray-tab-state", `${state.visible ? "Visible" : "Hidden"} · ${state.minimized ? "Minimized" : "Not minimized"} · Auto-start ${state.auto_start ? "on" : "off"}`);
  const as = el("tray-auto-start");
  if (as && state.auto_start !== undefined) as.checked = state.auto_start;
}

async function loadTrayTab() {
  if (!invoke) return;
  try {
    const state = await invoke("get_tray_state");
    updateTrayTabUI(state);
  } catch (e) {
    appendLog("[ERR] get_tray_state: " + e, "warn-line");
  }
}

async function applyTraySettings() {
  if (!invoke) return;
  const autoStart = el("tray-auto-start")?.checked || false;
  const minimize = el("tray-minimize")?.checked || false;
  try {
    await invoke("set_auto_start", { enabled: autoStart });
    if (currentConfig) {
      currentConfig.auto_start = autoStart;
      currentConfig.tray_minimize = minimize;
      currentConfig.close_to_tray = minimize;
      await pushConfig(currentConfig);
    }
    await loadTrayTab();
  } catch (e) {
    appendLog("[ERR] applyTray: " + e, "warn-line");
  }
}

async function setTrayVisibility(visible) {
  if (!invoke) return;
  try {
    await invoke("set_tray_state", { state: { visible, minimized: !visible, auto_start: el("tray-auto-start")?.checked || false } });
    await loadTrayTab();
  } catch (e) {
    appendLog("[ERR] set_tray_state: " + e, "warn-line");
  }
}

el("btn-tray-refresh")?.addEventListener("click", loadTrayTab);
el("btn-tray-apply")?.addEventListener("click", applyTraySettings);
el("btn-tray-show")?.addEventListener("click", () => setTrayVisibility(true));
el("btn-tray-hide")?.addEventListener("click", () => setTrayVisibility(false));
// Auto-apply toggle changes for immediate feedback.
el("tray-auto-start")?.addEventListener("change", applyTraySettings);
el("tray-minimize")?.addEventListener("change", applyTraySettings);

// =============================================================================
// Multi-controller tab
// =============================================================================
function renderControllerList(slots) {
  const body = el("multi-controller-list");
  if (!body) return;
  body.innerHTML = "";
  (slots || []).forEach((s, idx) => {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td>${idx}</td><td>${s.connected ? "Yes" : "No"}</td><td>${s.battery_percent ?? "—"}%</td><td>${escapeHtml(s.connection_type || "—")}</td><td>${escapeHtml(s.active_profile_name || "—")}</td>`;
    body.appendChild(tr);
  });
}

async function loadControllers() {
  if (!invoke) return;
  try {
    const slots = await invoke("get_controllers");
    renderControllerList(slots);
  } catch (e) {
    handleError("get_controllers failed", e);
  }
}

async function setActiveControllerSlot() {
  if (!invoke) return;
  const slot = parseInt(el("multi-slot")?.value || "0", 10);
  try {
    await invoke("set_active_slot", { slot });
    appendLog("[MULTI] Active slot set to " + slot, "hid-line");
  } catch (e) {
    handleError("set_active_slot failed", e);
  }
}

async function rescanControllers() {
  if (!invoke) return;
  try {
    await invoke("rescan_controllers");
    appendLog("[MULTI] Rescan requested", "hid-line");
  } catch (e) {
    handleError("rescan_controllers failed", e);
  }
}

el("btn-multi-list")?.addEventListener("click", loadControllers);
el("btn-multi-active")?.addEventListener("click", setActiveControllerSlot);
el("btn-multi-rescan")?.addEventListener("click", rescanControllers);
// Auto-apply slot selection when the dropdown changes.
el("multi-slot")?.addEventListener("change", setActiveControllerSlot);

// =============================================================================
// DSU Server tab
// =============================================================================
function updateDsuUI(status) {
  if (!status) return;
  setText("dsu-status", `${status.running ? "Running" : "Stopped"} · ${status.bind_address}:${status.port} @ ${status.update_rate_hz}Hz`);
  el("dsu-bind-address").value = status.bind_address;
  el("dsu-port").value = status.port;
  el("dsu-rate").value = status.update_rate_hz;
}

async function refreshDsuStatus() {
  if (!invoke) return;
  try {
    const status = await invoke("dsu_get_status");
    updateDsuUI(status);
  } catch (e) {
    appendLog("[ERR] dsu_get_status: " + e, "warn-line");
  }
}

async function updateDsuConfigFromInputs() {
  if (!currentConfig) return;
  currentConfig.dsu = currentConfig.dsu || {};
  currentConfig.dsu.bind_address = el("dsu-bind-address")?.value || "127.0.0.1";
  currentConfig.dsu.port = parseInt(el("dsu-port")?.value || "26760", 10);
  currentConfig.dsu.update_rate_hz = parseInt(el("dsu-rate")?.value || "60", 10);
  await pushConfig(currentConfig);
}

async function dsuStart() {
  if (!invoke) return;
  await updateDsuConfigFromInputs();
  try {
    const ok = await invoke("dsu_start");
    appendLog("[DSU] Start: " + ok, "hid-line");
    await refreshDsuStatus();
  } catch (e) {
    appendLog("[ERR] dsu_start: " + e, "warn-line");
  }
}

async function dsuStop() {
  if (!invoke) return;
  try {
    const ok = await invoke("dsu_stop");
    appendLog("[DSU] Stop: " + ok, "hid-line");
    await refreshDsuStatus();
  } catch (e) {
    appendLog("[ERR] dsu_stop: " + e, "warn-line");
  }
}

el("btn-dsu-start")?.addEventListener("click", dsuStart);
el("btn-dsu-stop")?.addEventListener("click", dsuStop);
el("btn-dsu-status")?.addEventListener("click", refreshDsuStatus);

// =============================================================================
// Flick Stick tab
// =============================================================================
async function loadFlick() {
  if (!invoke) return;
  try {
    const cfg = await invoke("get_flick_stick_config");
    el("flick-enabled").checked = !!cfg.enabled;
    el("flick-threshold").value = cfg.flick_threshold;
    el("flick-rotate-rate").value = cfg.rotate_rate_deg_per_sec;
    el("flick-deadzone").value = cfg.stick_deadzone;
    el("flick-cooldown").value = cfg.flick_cooldown_ms;
    el("flick-smoothing").value = cfg.output_smoothing;
  } catch (e) {
    appendLog("[ERR] get_flick_stick_config: " + e, "warn-line");
  }
}

async function applyFlick() {
  if (!invoke) return;
  const cfg = {
    enabled: el("flick-enabled")?.checked || false,
    flick_threshold: parseFloat(el("flick-threshold")?.value || "0.9"),
    rotate_rate_deg_per_sec: parseFloat(el("flick-rotate-rate")?.value || "360"),
    stick_deadzone: parseFloat(el("flick-deadzone")?.value || "0.15"),
    flick_cooldown_ms: parseInt(el("flick-cooldown")?.value || "150", 10),
    output_smoothing: parseFloat(el("flick-smoothing")?.value || "0"),
  };
  try {
    await invoke("set_flick_stick_config", { config: cfg });
  } catch (e) {
    appendLog("[ERR] set_flick_stick_config: " + e, "warn-line");
  }
}

async function resetFlickYaw() {
  if (!invoke) return;
  try {
    await invoke("reset_flick_stick_yaw", { slot: null });
  } catch (e) {
    appendLog("[ERR] reset_flick_stick_yaw: " + e, "warn-line");
  }
}

el("btn-flick-load")?.addEventListener("click", loadFlick);
el("btn-flick-apply")?.addEventListener("click", applyFlick);
el("btn-flick-reset-yaw")?.addEventListener("click", resetFlickYaw);

// =============================================================================
// NFC / amiibo tab
// =============================================================================
async function nfcGetState() {
  if (!invoke) return;
  try {
    const state = await invoke("get_nfc_state");
    setText("nfc-tab-status", JSON.stringify(state));
  } catch (e) {
    appendLog("[ERR] get_nfc_state: " + e, "warn-line");
  }
}

async function nfcSetEnabled(enabled) {
  if (!invoke) return;
  try {
    const state = await invoke("set_nfc_enabled", { enabled });
    setText("nfc-tab-status", JSON.stringify(state));
  } catch (e) {
    appendLog("[ERR] set_nfc_enabled: " + e, "warn-line");
  }
}

async function loadAmiibo() {
  if (!invoke) return;
  const path = el("nfc-amiibo-path")?.value?.trim();
  if (!path) return;
  try {
    const state = await invoke("load_amiibo_bin", { path });
    setText("nfc-tab-status", JSON.stringify(state));
  } catch (e) {
    appendLog("[ERR] load_amiibo_bin: " + e, "warn-line");
  }
}

async function emulateAmiibo() {
  if (!invoke) return;
  const path = el("nfc-amiibo-path")?.value?.trim();
  if (!path) return;
  try {
    const state = await invoke("emulate_amiibo_from_path", { path });
    setText("nfc-tab-status", JSON.stringify(state));
  } catch (e) {
    appendLog("[ERR] emulate_amiibo_from_path: " + e, "warn-line");
  }
}

el("nfc-tab-enabled")?.addEventListener("change", (e) => nfcSetEnabled(e.target.checked));
el("btn-nfc-get-state")?.addEventListener("click", nfcGetState);
el("btn-load-amiibo")?.addEventListener("click", loadAmiibo);
el("btn-emulate-amiibo")?.addEventListener("click", emulateAmiibo);

// =============================================================================
// Updater tab
// =============================================================================
async function getUpdateEndpoint() {
  if (!invoke) return;
  try {
    const endpoint = await invoke("get_update_endpoint");
    el("update-endpoint").value = endpoint;
  } catch (e) {
    appendLog("[ERR] get_update_endpoint: " + e, "warn-line");
  }
}

async function setUpdateEndpoint() {
  if (!invoke) return;
  try {
    await invoke("set_update_endpoint", { endpoint: el("update-endpoint")?.value || "" });
    appendLog("[UPDATER] Endpoint set", "hid-line");
  } catch (e) {
    appendLog("[ERR] set_update_endpoint: " + e, "warn-line");
  }
}

async function checkForUpdates() {
  if (!invoke) return;
  setText("updater-status", "Checking...");
  try {
    const info = await invoke("check_for_updates");
    if (info) {
      setText("updater-status", `v${info.version} available`);
    } else {
      setText("updater-status", "No update available");
    }
  } catch (e) {
    setText("updater-status", "Check failed");
    appendLog("[ERR] check_for_updates: " + e, "warn-line");
  }
}

async function installUpdate() {
  if (!invoke) return;
  setText("updater-status", "Installing...");
  try {
    const ok = await invoke("download_and_install_update");
    setText("updater-status", ok ? "Installed" : "No update");
  } catch (e) {
    setText("updater-status", "Install failed");
    appendLog("[ERR] download_and_install_update: " + e, "warn-line");
  }
}

el("btn-update-get-endpoint")?.addEventListener("click", getUpdateEndpoint);
el("btn-update-set-endpoint")?.addEventListener("click", setUpdateEndpoint);
el("btn-check-update")?.addEventListener("click", checkForUpdates);
el("btn-install-update")?.addEventListener("click", installUpdate);

// =============================================================================
// Telemetry / Privacy tab
// =============================================================================
async function loadTelemetry() {
  if (!invoke) return;
  try {
    const status = await invoke("get_telemetry_status");
    setText("telemetry-status", `Enabled: ${status.enabled}, Backend: ${status.backend}`);
    el("telemetry-enabled").checked = status.enabled;
  } catch (e) {
    appendLog("[ERR] get_telemetry_status: " + e, "warn-line");
  }
}

async function applyTelemetry() {
  if (!invoke) return;
  const enabled = el("telemetry-enabled")?.checked || false;
  const key = el("telemetry-key")?.value?.trim() || null;
  try {
    const status = await invoke("set_telemetry_enabled", { enabled, key });
    setText("telemetry-status", `Enabled: ${status.enabled}, Backend: ${status.backend}`);
  } catch (e) {
    appendLog("[ERR] set_telemetry_enabled: " + e, "warn-line");
  }
}

async function loadCrashReporting() {
  if (!invoke) return;
  try {
    const status = await invoke("get_crash_reporting_status");
    setText("crash-status", `Enabled: ${status.enabled}, Test: ${status.test_mode}`);
    el("crash-enabled").checked = status.enabled;
    if (status.dsn) el("crash-dsn").value = status.dsn;
  } catch (e) {
    appendLog("[ERR] get_crash_reporting_status: " + e, "warn-line");
  }
}

async function applyCrashReporting() {
  if (!invoke) return;
  const enabled = el("crash-enabled")?.checked || false;
  const dsn = el("crash-dsn")?.value?.trim() || null;
  try {
    const status = await invoke("set_crash_reporting", { enabled, dsn });
    setText("crash-status", `Enabled: ${status.enabled}, Test: ${status.test_mode}`);
  } catch (e) {
    appendLog("[ERR] set_crash_reporting: " + e, "warn-line");
  }
}

async function recordTelemetryEvent() {
  if (!invoke) return;
  const name = el("telemetry-event-name")?.value?.trim() || "test_event";
  try {
    const ok = await invoke("record_telemetry_event", { name, payload: { source: "frontend" } });
    appendLog("[TELEMETRY] Recorded " + name + ": " + ok, "hid-line");
  } catch (e) {
    appendLog("[ERR] record_telemetry_event: " + e, "warn-line");
  }
}

el("btn-telemetry-apply")?.addEventListener("click", applyTelemetry);
el("btn-crash-apply")?.addEventListener("click", applyCrashReporting);
el("btn-record-telemetry")?.addEventListener("click", recordTelemetryEvent);

// =============================================================================
// Overlay
// =============================================================================
async function loadOverlay() {
  if (!invoke) return;
  try {
    const cfg = await invoke("get_overlay_config");
    el("overlay-enabled").checked = cfg.enabled;
    el("overlay-hotkey").value = cfg.toggle_hotkey || "Shift+F11";
    el("overlay-opacity").value = cfg.opacity ?? 0.9;
    el("overlay-position").value = cfg.position || "top-left";
    el("overlay-scale").value = cfg.scale ?? 1;
    el("overlay-show-battery").checked = cfg.show_battery ?? true;
    el("overlay-show-profile").checked = cfg.show_profile ?? true;
    el("overlay-show-fps").checked = cfg.show_fps ?? false;
  } catch (e) {
    appendLog("[ERR] loadOverlay: " + e, "warn-line");
  }
}

async function saveOverlay() {
  if (!invoke) return;
  try {
    const cfg = await invoke("get_overlay_config");
    const enabled = el("overlay-enabled").checked;
    const toggle_hotkey = el("overlay-hotkey").value.trim() || "Shift+F11";
    const opacity = parseFloat(el("overlay-opacity").value);
    const position = el("overlay-position").value;
    const scale = parseFloat(el("overlay-scale").value);
    const show_battery = el("overlay-show-battery").checked;
    const show_profile = el("overlay-show-profile").checked;
    const show_fps = el("overlay-show-fps").checked;
    await invoke("set_overlay_config", {
      ...cfg,
      enabled,
      toggle_hotkey,
      opacity,
      position,
      scale,
      show_battery,
      show_profile,
      show_fps,
    });
    appendLog("[OVERLAY] Config saved", "hid-line");
  } catch (e) {
    appendLog("[ERR] saveOverlay: " + e, "warn-line");
  }
}

async function toggleOverlay() {
  if (!invoke) return;
  try {
    const visible = await invoke("toggle_overlay");
    appendLog("[OVERLAY] Visible: " + visible, "hid-line");
  } catch (e) {
    appendLog("[ERR] toggleOverlay: " + e, "warn-line");
  }
}

el("btn-overlay-load")?.addEventListener("click", loadOverlay);
el("btn-overlay-save")?.addEventListener("click", saveOverlay);
el("btn-overlay-toggle")?.addEventListener("click", toggleOverlay);

// =============================================================================
// Cloud
// =============================================================================
async function loadCloud() {
  if (!invoke) return;
  try {
    const cfg = await invoke("get_cloud_config");
    el("cloud-enabled").checked = cfg.enabled;
    el("cloud-endpoint").value = cfg.endpoint || "";
    el("cloud-api-key").value = cfg.api_key || "";
    el("cloud-username").value = cfg.username || "";
    el("cloud-accepted-terms").checked = cfg.accepted_terms ?? false;
  } catch (e) {
    appendLog("[ERR] loadCloud: " + e, "warn-line");
  }
}

async function saveCloud() {
  if (!invoke) return;
  try {
    const cfg = await invoke("get_cloud_config");
    const enabled = el("cloud-enabled").checked;
    const endpoint = el("cloud-endpoint").value.trim();
    const api_key = el("cloud-api-key").value.trim() || null;
    const username = el("cloud-username").value.trim();
    const accepted_terms = el("cloud-accepted-terms").checked;
    await invoke("set_cloud_config", { ...cfg, enabled, endpoint, api_key, username, accepted_terms });
    appendLog("[CLOUD] Config saved", "hid-line");
  } catch (e) {
    appendLog("[ERR] saveCloud: " + e, "warn-line");
  }
}

async function listCommunityProfiles() {
  if (!invoke) return;
  try {
    const profiles = await invoke("list_community_profiles", { tags: "" });
    const list = el("cloud-list");
    if (list) {
      list.innerHTML = "";
      if (!profiles || profiles.length === 0) {
        const empty = document.createElement("div");
        empty.className = "metric-label";
        empty.textContent = "No community profiles found.";
        list.appendChild(empty);
      } else {
        profiles.forEach((p) => {
          const row = document.createElement("div");
          row.className = "glass panel";
          row.style.cssText = "padding:8px 12px;display:flex;align-items:center;gap:8px;flex-wrap:wrap";
          const info = document.createElement("span");
          info.style.flex = "1";
          info.innerHTML = `<strong>${escapeHtml(p.name)}</strong> by ${escapeHtml(p.author)}` +
            (p.description ? ` — ${escapeHtml(p.description)}` : "") +
            ` · ${p.downloads ?? 0} downloads` +
            (p.rating ? ` · ★ ${p.rating.toFixed(1)}` : "");
          const dlBtn = document.createElement("button");
          dlBtn.className = "btn";
          dlBtn.textContent = "Download";
          dlBtn.addEventListener("click", () => downloadProfileById(p.id, p.name));
          row.appendChild(info);
          row.appendChild(dlBtn);
          list.appendChild(row);
        });
      }
    }
    appendLog("[CLOUD] Listed " + (profiles?.length || 0) + " profiles", "hid-line");
  } catch (e) {
    handleError("listCommunityProfiles failed", e);
  }
}

async function downloadProfileById(id, name) {
  if (!invoke) return;
  try {
    const profile = await invoke("download_profile", { id });
    appendLog("[CLOUD] Downloaded " + (profile?.name || name || id), "hid-line");
  } catch (e) {
    handleError("downloadProfileById failed", e);
  }
}

async function uploadActiveProfile() {
  if (!invoke) return;
  try {
    const profile = await invoke("get_active_profile");
    if (!profile) {
      appendLog("[CLOUD] No active profile to upload", "warn-line");
      return;
    }
    const code = await invoke("upload_profile", { profile });
    appendLog("[CLOUD] Uploaded, share code: " + code, "hid-line");
  } catch (e) {
    handleError("uploadActiveProfile failed", e);
  }
}

async function downloadByCode() {
  if (!invoke) return;
  try {
    const code = el("cloud-share-code")?.value?.trim();
    if (!code) return;
    const profile = await invoke("get_profile_by_code", { code });
    appendLog("[CLOUD] Downloaded " + (profile?.name || code), "hid-line");
  } catch (e) {
    handleError("downloadByCode failed", e);
  }
}

el("btn-cloud-load")?.addEventListener("click", loadCloud);
el("btn-cloud-save")?.addEventListener("click", saveCloud);
el("btn-cloud-list")?.addEventListener("click", listCommunityProfiles);
el("btn-cloud-upload")?.addEventListener("click", uploadActiveProfile);
el("btn-cloud-download")?.addEventListener("click", downloadByCode);

// Tab-switch loaders
function onTabActivated(tab) {
  if (tab === "macros") loadMacros();
  if (tab === "curves") { loadCurve(); loadZones(); }
  if (tab === "kbm") loadKbm();
  if (tab === "hidhide") refreshHidHideTabStatus();
  if (tab === "logging") { refreshLoggingLogs(); }
  if (tab === "tray") loadTrayTab();
  if (tab === "multi") loadControllers();
  if (tab === "dsu") refreshDsuStatus();
  if (tab === "flick") loadFlick();
  if (tab === "nfc") nfcGetState();
  if (tab === "updater") getUpdateEndpoint();
  if (tab === "telemetry") { loadTelemetry(); loadCrashReporting(); }
  if (tab === "overlay") loadOverlay();
  if (tab === "cloud") loadCloud();
}

document.querySelectorAll(".tab-btn").forEach((btn) => {
  const old = btn.onclick;
  btn.addEventListener("click", () => onTabActivated(btn.dataset.tab));
});

if (invoke) {
  loadMacros();
  loadProfiles();
  // Load full config once to set up settings UI (including virtual controller type).
  (async () => {
    try {
      const cfg = await invoke("get_config");
      if (cfg) {
        currentConfig = cfg;
        updateConfig(cfg);
        updateSettingsUI(cfg);
      }
    } catch (e) {
      /* ignore — config may not be available yet */
    }
    refreshTrayState();
    loadKbm();
  })();
}

// "Switch to BT" button — triggers a Bluetooth reconnect for the paired
// Pro Controller. Useful for switching from USB to Bluetooth without
// physically unplugging the cable.
if (el("btn-bt-reconnect")) {
  el("btn-bt-reconnect").addEventListener("click", async () => {
    if (!invoke) return;
    appendLog("[INFO] Triggering Bluetooth reconnect…", "hid-line");
    try {
      const ok = await invoke("trigger_bt_reconnect");
      if (ok) {
        appendLog("[OK] Bluetooth reconnect triggered — controller should reconnect over BT shortly", "hid-line");
      } else {
        appendLog("[WARN] Bluetooth reconnect failed — is the controller paired with Windows?", "warn-line");
      }
    } catch (err) {
      appendLog("[ERR] trigger_bt_reconnect failed: " + err, "warn-line");
    }
  });
}
