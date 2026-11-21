/**
 * Converts a key press into the engine's hotkey syntax ("Ctrl+Shift+S", "F14", "Num7").
 * Only keys the engine can register are accepted (see crates/engine/src/hotkey.rs).
 */
const NAMED: Record<string, string> = {
  Home: "Home",
  End: "End",
  Insert: "Insert",
  Delete: "Delete",
  Pause: "Pause",
  PrintScreen: "PrintScreen",
  ScrollLock: "ScrollLock",
  PageUp: "PageUp",
  PageDown: "PageDown",
  NumpadAdd: "NumAdd",
  NumpadSubtract: "NumSubtract",
  NumpadMultiply: "NumMultiply",
  NumpadDivide: "NumDivide",
  NumpadDecimal: "NumDecimal",
};

const MODIFIER_CODES = new Set([
  "ControlLeft",
  "ControlRight",
  "ShiftLeft",
  "ShiftRight",
  "AltLeft",
  "AltRight",
  "MetaLeft",
  "MetaRight",
]);

export type Capture = { combo: string } | { error: string } | { pending: true };

function keyName(code: string): string | null {
  let m = /^F([1-9]|1[0-9]|2[0-4])$/.exec(code);
  if (m) return code;
  m = /^Key([A-Z])$/.exec(code);
  if (m) return m[1];
  m = /^Digit([0-9])$/.exec(code);
  if (m) return m[1];
  m = /^Numpad([0-9])$/.exec(code);
  if (m) return `Num${m[1]}`;
  return NAMED[code] ?? null;
}

export function captureHotkey(e: KeyboardEvent): Capture {
  if (MODIFIER_CODES.has(e.code)) return { pending: true };
  const key = keyName(e.code);
  if (!key) return { error: `${e.key === " " ? "Space" : e.key || e.code} can't be used as a hotkey` };
  const mods = [e.ctrlKey && "Ctrl", e.altKey && "Alt", e.shiftKey && "Shift", e.metaKey && "Win"].filter(Boolean);
  // Plain letters and digits would fire while typing in chat; require a modifier for them.
  if (mods.length === 0 && /^[A-Z0-9]$/.test(key)) return { error: `Add Ctrl, Alt or Shift to use ${key}` };
  return { combo: [...mods, key].join("+") };
}

export const hotkeyParts = (combo: string) => combo.split("+").filter(Boolean);
