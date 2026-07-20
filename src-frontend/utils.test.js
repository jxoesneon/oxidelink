import { describe, it, expect, beforeEach } from "vitest";
import {
  escapeHtml,
  formatLogTimestamp,
  logLevelClass,
  buildBindingAction,
  parseBindingAction,
  RollingAverage,
  pushBuffer,
  buildLedMask,
  getCheckedLedIndices,
} from "./utils.js";

describe("escapeHtml", () => {
  it("escapes all HTML special characters", () => {
    expect(escapeHtml(`<div class="x">&</div>`)).toBe(
      "&lt;div class=&quot;x&quot;&gt;&amp;&lt;/div&gt;"
    );
  });
  it("escapes single quotes", () => {
    expect(escapeHtml("it's")).toBe("it&#39;s");
  });
  it("passes through non-HTML strings unchanged", () => {
    expect(escapeHtml("hello world")).toBe("hello world");
  });
  it("coerces non-string input to string", () => {
    expect(escapeHtml(42)).toBe("42");
  });
});

describe("formatLogTimestamp", () => {
  it("formats a UTC timestamp as YYYY-MM-DD HH:MM:SS", () => {
    const ts = Date.UTC(2026, 6, 20, 12, 30, 45);
    expect(formatLogTimestamp(ts)).toBe("2026-07-20 12:30:45");
  });
  it("drops milliseconds", () => {
    const ts = Date.UTC(2026, 0, 1, 0, 0, 0, 500);
    expect(formatLogTimestamp(ts)).toBe("2026-01-01 00:00:00");
  });
});

describe("logLevelClass", () => {
  it("returns the correct CSS class for each level", () => {
    expect(logLevelClass("error")).toBe("level-error");
    expect(logLevelClass("warn")).toBe("level-warn");
    expect(logLevelClass("info")).toBe("level-info");
    expect(logLevelClass("debug")).toBe("level-debug");
    expect(logLevelClass("trace")).toBe("level-trace");
  });
  it("is case-insensitive", () => {
    expect(logLevelClass("ERROR")).toBe("level-error");
    expect(logLevelClass("Info")).toBe("level-info");
  });
  it("returns empty string for unknown levels", () => {
    expect(logLevelClass("verbose")).toBe("");
    expect(logLevelClass(null)).toBe("");
    expect(logLevelClass(undefined)).toBe("");
  });
});

describe("buildBindingAction", () => {
  it("builds a turbo action", () => {
    expect(buildBindingAction("turbo", "a", 50)).toEqual({
      type: "turbo",
      value: { button: "a", interval_ms: 50 },
    });
  });
  it("builds a toggle action", () => {
    expect(buildBindingAction("toggle", "b", 100)).toEqual({
      type: "toggle",
      value: { button: "b" },
    });
  });
  it("builds a normal button action for unknown mode", () => {
    expect(buildBindingAction("normal", "x", 100)).toEqual({
      type: "button",
      value: "x",
    });
  });
});

describe("parseBindingAction", () => {
  it("returns defaults for null/undefined", () => {
    expect(parseBindingAction(null)).toEqual({
      mode: "normal",
      target: "a",
      interval: 100,
    });
  });
  it("parses turbo action", () => {
    const action = { type: "turbo", value: { button: "y", interval_ms: 75 } };
    expect(parseBindingAction(action)).toEqual({
      mode: "turbo",
      target: "y",
      interval: 75,
    });
  });
  it("parses toggle action", () => {
    const action = { type: "toggle", value: { button: "l" } };
    expect(parseBindingAction(action)).toEqual({
      mode: "toggle",
      target: "l",
      interval: 100,
    });
  });
  it("parses button action", () => {
    const action = { type: "button", value: "r" };
    expect(parseBindingAction(action)).toEqual({
      mode: "normal",
      target: "r",
      interval: 100,
    });
  });
  it("falls back to defaults for unknown type", () => {
    expect(parseBindingAction({ type: "unknown" })).toEqual({
      mode: "normal",
      target: "a",
      interval: 100,
    });
  });
  it("handles missing button in turbo value", () => {
    expect(parseBindingAction({ type: "turbo", value: {} })).toEqual({
      mode: "turbo",
      target: "a",
      interval: 100,
    });
  });
});

describe("RollingAverage", () => {
  it("returns null for empty average", () => {
    const ra = new RollingAverage();
    expect(ra.avg()).toBeNull();
    expect(ra.median()).toBeNull();
  });
  it("computes average of pushed values", () => {
    const ra = new RollingAverage(10000, Infinity);
    ra.push(10);
    ra.push(20);
    ra.push(30);
    expect(ra.avg()).toBeCloseTo(20);
  });
  it("computes median of pushed values", () => {
    const ra = new RollingAverage(10000, Infinity);
    ra.push(10);
    ra.push(20);
    ra.push(30);
    expect(ra.median()).toBe(20);
  });
  it("suppresses spikes when enough samples exist", () => {
    const ra = new RollingAverage(10000, 3.0);
    // Use varied values so MAD is non-zero
    ra.push(98);
    ra.push(99);
    ra.push(100);
    ra.push(101);
    ra.push(102);
    const beforeSpike = ra.samples.length;
    ra.push(10000); // spike should be rejected
    expect(ra.samples.length).toBe(beforeSpike);
  });
  it("allows samples when MAD is zero", () => {
    const ra = new RollingAverage(10000, 3.0);
    for (let i = 0; i < 5; i++) ra.push(50);
    ra.push(50);
    expect(ra.samples.length).toBe(6);
  });
});

describe("pushBuffer", () => {
  it("appends values to the buffer", () => {
    const buf = [];
    pushBuffer(buf, 1);
    pushBuffer(buf, 2);
    expect(buf).toEqual([1, 2]);
  });

  it("trims the buffer to maxSize", () => {
    const buf = [];
    for (let i = 0; i < 10; i++) pushBuffer(buf, i, 5);
    expect(buf.length).toBe(5);
    expect(buf).toEqual([5, 6, 7, 8, 9]);
  });

  it("defaults maxSize to 100", () => {
    const buf = [];
    for (let i = 0; i < 150; i++) pushBuffer(buf, i);
    expect(buf.length).toBe(100);
    expect(buf[0]).toBe(50);
  });
});

describe("buildLedMask", () => {
  let mockGet;
  beforeEach(() => {
    mockGet = (id) => {
      const toggles = {
        "led-toggle-1": { checked: true },
        "led-toggle-2": { checked: false },
        "led-toggle-3": { checked: true },
        "led-toggle-4": { checked: false },
      };
      return toggles[id];
    };
  });

  it("builds a bitmask from checked toggles", () => {
    expect(buildLedMask(mockGet)).toBe(0b0101); // bits 0 and 2
  });

  it("returns 0 when no toggles are checked", () => {
    const allUnchecked = () => ({ checked: false });
    expect(buildLedMask(allUnchecked)).toBe(0);
  });

  it("returns 0b1111 when all toggles are checked", () => {
    const allChecked = () => ({ checked: true });
    expect(buildLedMask(allChecked)).toBe(0b1111);
  });

  it("handles missing elements gracefully", () => {
    const noElements = () => null;
    expect(buildLedMask(noElements)).toBe(0);
  });
});

describe("getCheckedLedIndices", () => {
  it("returns 1-based indices of checked toggles", () => {
    const mockGet = (id) => {
      const toggles = {
        "led-toggle-1": { checked: true },
        "led-toggle-2": { checked: false },
        "led-toggle-3": { checked: true },
        "led-toggle-4": { checked: true },
      };
      return toggles[id];
    };
    expect(getCheckedLedIndices(mockGet)).toEqual([1, 3, 4]);
  });

  it("returns empty array when none checked", () => {
    const allUnchecked = () => ({ checked: false });
    expect(getCheckedLedIndices(allUnchecked)).toEqual([]);
  });

  it("returns all four when all checked", () => {
    const allChecked = () => ({ checked: true });
    expect(getCheckedLedIndices(allChecked)).toEqual([1, 2, 3, 4]);
  });

  it("handles missing elements gracefully", () => {
    const noElements = () => null;
    expect(getCheckedLedIndices(noElements)).toEqual([]);
  });
});
