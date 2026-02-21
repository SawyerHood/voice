export const DEFAULT_HOTKEY_SHORTCUT = "Alt+Space";
export const OPENAI_PROVIDER = "openai";

export type RecordingMode = "hold_to_talk" | "toggle" | "double_tap_toggle";

type ShortcutCaptureEvent = Pick<
  KeyboardEvent,
  "key" | "code" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey" | "location" | "getModifierState"
>;

type SettingsUpdateInput = {
  hotkeyShortcut: string;
  recordingMode: RecordingMode;
  microphoneId: string;
  language: string;
  autoInsert: boolean;
  launchAtLogin: boolean;
};

export type VoiceSettingsUpdatePayload = {
  hotkey_shortcut: string;
  recording_mode: RecordingMode;
  microphone_id: string | null;
  language: string | null;
  transcription_provider: typeof OPENAI_PROVIDER;
  auto_insert: boolean;
  launch_at_login: boolean;
};

export function normalizeShortcut(shortcut: string): string {
  const trimmed = shortcut.trim();
  return trimmed.length > 0 ? trimmed : DEFAULT_HOTKEY_SHORTCUT;
}

export function normalizeOptionalText(value: string): string | null {
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

export function normalizeRecordingMode(value: string): RecordingMode {
  if (value === "toggle") return "toggle";
  if (value === "double_tap_toggle") return "double_tap_toggle";
  return "hold_to_talk";
}

export function createSettingsUpdatePayload(
  input: SettingsUpdateInput,
): VoiceSettingsUpdatePayload {
  return {
    hotkey_shortcut: normalizeShortcut(input.hotkeyShortcut),
    recording_mode: input.recordingMode,
    microphone_id: normalizeOptionalText(input.microphoneId),
    language: normalizeOptionalText(input.language),
    transcription_provider: OPENAI_PROVIDER,
    auto_insert: input.autoInsert,
    launch_at_login: input.launchAtLogin,
  };
}

const MODIFIER_CODES = new Set([
  "ShiftLeft",
  "ShiftRight",
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "MetaLeft",
  "MetaRight",
  "Fn",
  "FnLeft",
  "FnRight",
]);

const MODIFIER_KEYS = new Set([
  "Shift",
  "Control",
  "Alt",
  "Meta",
  "OS",
  "LShift",
  "RShift",
  "LCtrl",
  "RCtrl",
  "LAlt",
  "RAlt",
  "LMeta",
  "RMeta",
  "Cmd",
  "Fn",
]);

const DOM_KEY_LOCATION_STANDARD = globalThis.KeyboardEvent?.DOM_KEY_LOCATION_STANDARD ?? 0;
const DOM_KEY_LOCATION_LEFT = globalThis.KeyboardEvent?.DOM_KEY_LOCATION_LEFT ?? 1;
const DOM_KEY_LOCATION_RIGHT = globalThis.KeyboardEvent?.DOM_KEY_LOCATION_RIGHT ?? 2;

const CODE_ALIASES: Record<string, string> = {
  Space: "Space",
  Escape: "Escape",
  Enter: "Enter",
  Tab: "Tab",
  Backspace: "Backspace",
  Delete: "Delete",
  Insert: "Insert",
  Home: "Home",
  End: "End",
  PageUp: "PageUp",
  PageDown: "PageDown",
  ArrowUp: "ArrowUp",
  ArrowDown: "ArrowDown",
  ArrowLeft: "ArrowLeft",
  ArrowRight: "ArrowRight",
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: ",",
  Period: ".",
  Slash: "/",
  Backquote: "`",
  Numpad0: "Numpad0",
  Numpad1: "Numpad1",
  Numpad2: "Numpad2",
  Numpad3: "Numpad3",
  Numpad4: "Numpad4",
  Numpad5: "Numpad5",
  Numpad6: "Numpad6",
  Numpad7: "Numpad7",
  Numpad8: "Numpad8",
  Numpad9: "Numpad9",
  NumpadAdd: "NumpadAdd",
  NumpadSubtract: "NumpadSubtract",
  NumpadMultiply: "NumpadMultiply",
  NumpadDivide: "NumpadDivide",
  NumpadDecimal: "NumpadDecimal",
  NumpadEnter: "NumpadEnter",
  NumpadEqual: "NumpadEqual",
};

function resolveShortcutKey(event: ShortcutCaptureEvent): string | null {
  if (MODIFIER_CODES.has(event.code) || MODIFIER_KEYS.has(event.key)) {
    return null;
  }

  if (/^Key[A-Z]$/i.test(event.code)) {
    return event.code.slice(3).toUpperCase();
  }

  if (/^Digit[0-9]$/.test(event.code)) {
    return event.code.slice(5);
  }

  if (/^F[0-9]{1,2}$/i.test(event.code)) {
    return event.code.toUpperCase();
  }

  const mappedCode = CODE_ALIASES[event.code];
  if (mappedCode) {
    return mappedCode;
  }

  if (event.key === " ") {
    return "Space";
  }

  if (/^[a-z]$/i.test(event.key)) {
    return event.key.toUpperCase();
  }

  if (/^[0-9]$/.test(event.key)) {
    return event.key;
  }

  return null;
}

export function shortcutFromKeyboardEvent(event: ShortcutCaptureEvent): string | null {
  const key = resolveShortcutKey(event);
  if (!key) return null;

  const location = event.location ?? DOM_KEY_LOCATION_STANDARD;
  const modifiers: string[] = [];
  if (event.ctrlKey) modifiers.push(locationAwareModifier("Ctrl", location));
  if (event.altKey) modifiers.push(locationAwareModifier("Alt", location));
  if (event.shiftKey) modifiers.push(locationAwareModifier("Shift", location));
  if (event.metaKey) modifiers.push(locationAwareMetaModifier(location));
  if (isFnModifierPressed(event)) modifiers.push("Fn");
  modifiers.push(key);

  return modifiers.join("+");
}

function locationAwareModifier(
  modifier: "Ctrl" | "Alt" | "Shift",
  location: number,
): string {
  if (location === DOM_KEY_LOCATION_LEFT) {
    return `L${modifier}`;
  }

  if (location === DOM_KEY_LOCATION_RIGHT) {
    return `R${modifier}`;
  }

  return modifier;
}

function locationAwareMetaModifier(location: number): string {
  if (location === DOM_KEY_LOCATION_LEFT) {
    return "LMeta";
  }

  if (location === DOM_KEY_LOCATION_RIGHT) {
    return "RMeta";
  }

  return "Cmd";
}

function isFnModifierPressed(event: ShortcutCaptureEvent): boolean {
  if (typeof event.getModifierState !== "function") {
    return false;
  }

  return event.getModifierState("Fn");
}

export function maskApiKey(key: string): string {
  const trimmed = key.trim();
  if (!trimmed) {
    return "";
  }

  const maskLength = Math.max(8, Math.min(24, trimmed.length));
  return "•".repeat(maskLength);
}
