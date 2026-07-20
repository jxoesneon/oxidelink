// OxideLink frontend — pure utility functions extracted for unit testing.
// These functions have no DOM dependencies and are safe to import in Vitest.

export function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
}

export function formatLogTimestamp(ts) {
  const d = new Date(ts);
  return d.toISOString().replace("T", " ").split(".")[0];
}

export function logLevelClass(level) {
  switch ((level || "").toLowerCase()) {
    case "error": return "level-error";
    case "warn": return "level-warn";
    case "info": return "level-info";
    case "debug": return "level-debug";
    case "trace": return "level-trace";
    default: return "";
  }
}

export function buildBindingAction(mode, target, interval) {
  if (mode === "turbo") {
    return { type: "turbo", value: { button: target, interval_ms: interval } };
  }
  if (mode === "toggle") {
    return { type: "toggle", value: { button: target } };
  }
  return { type: "button", value: target };
}

export function parseBindingAction(action) {
  if (!action) return { mode: "normal", target: "a", interval: 100 };
  if (action.type === "turbo" && action.value) {
    return { mode: "turbo", target: action.value.button || "a", interval: action.value.interval_ms ?? 100 };
  }
  if (action.type === "toggle" && action.value) {
    return { mode: "toggle", target: action.value.button || "a", interval: 100 };
  }
  if (action.type === "button") {
    return { mode: "normal", target: action.value || "a", interval: 100 };
  }
  return { mode: "normal", target: "a", interval: 100 };
}

// RollingAverage — 1s rolling average with spike suppression for jittery telemetry.
export class RollingAverage {
  constructor(windowMs = 1000, maxDeviation = 3.0) {
    this.windowMs = windowMs;
    this.maxDeviation = maxDeviation;
    this.samples = [];
  }
  push(value) {
    const now = performance.now();
    if (this.samples.length >= 5) {
      const median = this._median();
      const mad = this._mad(median);
      if (mad > 0 && Math.abs(value - median) > this.maxDeviation * mad * 1.4826) {
        return;
      }
    }
    this.samples.push({ value, time: now });
    const cutoff = now - this.windowMs;
    while (this.samples.length && this.samples[0].time < cutoff) this.samples.shift();
  }
  _median() {
    const vals = this.samples.map(s => s.value).sort((a, b) => a - b);
    const n = vals.length;
    return n % 2 ? vals[(n - 1) >> 1] : (vals[n / 2 - 1] + vals[n / 2]) / 2;
  }
  _mad(median) {
    const devs = this.samples.map(s => Math.abs(s.value - median)).sort((a, b) => a - b);
    const n = devs.length;
    return n % 2 ? devs[(n - 1) >> 1] : (devs[n / 2 - 1] + devs[n / 2]) / 2;
  }
  avg() {
    if (!this.samples.length) return null;
    let sum = 0;
    for (const s of this.samples) sum += s.value;
    return sum / this.samples.length;
  }
  median() {
    if (!this.samples.length) return null;
    return this._median();
  }
}

// pushBuffer — append a value to a fixed-size ring buffer, dropping from the front.
export function pushBuffer(buffer, value, maxSize = 100) {
  buffer.push(value);
  while (buffer.length > maxSize) buffer.shift();
  return buffer;
}

// buildLedMask — read 4 LED toggle checkboxes and build a 4-bit bitmask.
// Accepts an optional `getElementById` function for testability (defaults to document.getElementById).
export function buildLedMask(getById = (id) => document.getElementById(id)) {
  let mask = 0;
  for (let i = 1; i <= 4; i++) {
    const tog = getById(`led-toggle-${i}`);
    if (tog && tog.checked) mask |= 1 << (i - 1);
  }
  return mask;
}

// getCheckedLedIndices — return the 1-based indices of checked LED toggles.
export function getCheckedLedIndices(getById = (id) => document.getElementById(id)) {
  const indices = [];
  for (let i = 1; i <= 4; i++) {
    const tog = getById(`led-toggle-${i}`);
    if (tog && tog.checked) indices.push(i);
  }
  return indices;
}

