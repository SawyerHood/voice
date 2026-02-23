import { describe, expect, it } from "vitest";

import {
  DEFAULT_HOTKEY_SHORTCUT,
  createSettingsUpdatePayload,
  maskApiKey,
  normalizeOptionalText,
  normalizeRecordingMode,
  normalizeShortcut,
  shortcutFromKeyboardEvent,
} from "./settingsUtils";

function keyboardEvent(input: Partial<KeyboardEvent>): KeyboardEvent {
  return {
    key: "",
    code: "",
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    metaKey: false,
    ...input,
  } as KeyboardEvent;
}

describe("settingsUtils", () => {
  it("uses default shortcut when the input is blank", () => {
    expect(normalizeShortcut("   ")).toBe(DEFAULT_HOTKEY_SHORTCUT);
  });

  it("normalizes optional text values", () => {
    expect(normalizeOptionalText("  en  ")).toBe("en");
    expect(normalizeOptionalText("   ")).toBeNull();
  });

  it("normalizes recording mode with a safe fallback", () => {
    expect(normalizeRecordingMode("toggle")).toBe("toggle");
    expect(normalizeRecordingMode("legacy_mode")).toBe("hold_to_talk");
    expect(normalizeRecordingMode("anything-else")).toBe("hold_to_talk");
  });

  it("builds settings update payloads that match backend expectations", () => {
    expect(
      createSettingsUpdatePayload({
        hotkeyShortcut: "  Cmd+Shift+Space ",
        recordingMode: "toggle",
        microphoneId: "  mic-1 ",
        language: "  fr ",
        autoInsert: false,
        launchAtLogin: true,
      }),
    ).toEqual({
      hotkey_shortcut: "Cmd+Shift+Space",
      recording_mode: "toggle",
      microphone_id: "mic-1",
      language: "fr",
      transcription_provider: "openai",
      auto_insert: false,
      launch_at_login: true,
    });
  });

  it("masks API keys with bounded bullet length", () => {
    expect(maskApiKey("sk-short")).toBe("••••••••");
    expect(maskApiKey("a".repeat(40))).toBe("•".repeat(24));
    expect(maskApiKey("   ")).toBe("");
  });

  it("captures modifier shortcuts from keyboard events", () => {
    expect(
      shortcutFromKeyboardEvent(
        keyboardEvent({
          key: "s",
          code: "KeyS",
          ctrlKey: true,
          shiftKey: true,
        }),
      ),
    ).toBe("Ctrl+Shift+S");
  });

  it("captures right alt as the Alt modifier", () => {
    expect(
      shortcutFromKeyboardEvent(
        keyboardEvent({
          key: " ",
          code: "Space",
          altKey: true,
        }),
      ),
    ).toBe("Alt+Space");
  });

  it("captures command shortcuts with Cmd token", () => {
    expect(
      shortcutFromKeyboardEvent(
        keyboardEvent({
          key: " ",
          code: "Space",
          metaKey: true,
        }),
      ),
    ).toBe("Cmd+Space");
  });

  it("ignores pure modifier presses while recording shortcuts", () => {
    expect(
      shortcutFromKeyboardEvent(
        keyboardEvent({
          key: "Shift",
          code: "ShiftLeft",
          shiftKey: true,
        }),
      ),
    ).toBeNull();
  });
});
