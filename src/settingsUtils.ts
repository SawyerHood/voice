export const DEFAULT_HOTKEY_SHORTCUT = "Alt+Space";
export const OPENAI_PROVIDER = "openai";

export type RecordingMode = "hold_to_talk" | "toggle";

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
  return value === "toggle" ? "toggle" : "hold_to_talk";
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

export function maskApiKey(key: string): string {
  const trimmed = key.trim();
  if (!trimmed) {
    return "";
  }

  const maskLength = Math.max(8, Math.min(24, trimmed.length));
  return "•".repeat(maskLength);
}
