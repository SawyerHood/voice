import { type FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

import {
  createSettingsUpdatePayload,
  normalizeRecordingMode,
  normalizeShortcut,
  OPENAI_PROVIDER,
  type RecordingMode,
} from "./settingsUtils";

type VoiceSettings = {
  hotkey_shortcut: string;
  recording_mode: string;
  microphone_id: string | null;
  language: string | null;
  auto_insert: boolean;
  launch_at_login: boolean;
};

type HotkeyConfig = {
  shortcut: string;
  mode: RecordingMode;
};

type MicrophoneInfo = {
  id: string;
  name: string;
  isDefault: boolean;
  sampleRateHz: number | null;
  channels: number | null;
};

type SaveFeedback = {
  kind: "success" | "error";
  message: string;
};

function toErrorMessage(error: unknown, fallback: string): string {
  if (typeof error === "string") {
    const trimmed = error.trim();
    if (trimmed.length > 0) {
      return trimmed;
    }
  }

  if (error instanceof Error) {
    const trimmed = error.message.trim();
    if (trimmed.length > 0) {
      return trimmed;
    }
  }

  return fallback;
}

function formatMicrophoneLabel(device: MicrophoneInfo): string {
  const details: string[] = [];
  if (device.isDefault) {
    details.push("Default");
  }

  if (device.sampleRateHz) {
    details.push(`${device.sampleRateHz} Hz`);
  }

  if (device.channels) {
    details.push(`${device.channels} ch`);
  }

  if (details.length === 0) {
    return device.name;
  }

  return `${device.name} (${details.join(", ")})`;
}

export default function Settings() {
  const [isLoading, setIsLoading] = useState(true);
  const [isSavingSettings, setIsSavingSettings] = useState(false);
  const [isSavingApiKey, setIsSavingApiKey] = useState(false);
  const [isRefreshingMics, setIsRefreshingMics] = useState(false);
  const [isExportingLogs, setIsExportingLogs] = useState(false);
  const [feedback, setFeedback] = useState<SaveFeedback | null>(null);

  const [hotkeyShortcut, setHotkeyShortcut] = useState("");
  const [recordingMode, setRecordingMode] = useState<RecordingMode>("hold_to_talk");
  const [microphones, setMicrophones] = useState<MicrophoneInfo[]>([]);
  const [microphoneId, setMicrophoneId] = useState("");
  const [language, setLanguage] = useState("");
  const [autoInsert, setAutoInsert] = useState(true);
  const [launchAtLogin, setLaunchAtLogin] = useState(false);

  const [hasStoredApiKey, setHasStoredApiKey] = useState(false);
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [isApiKeyDraftVisible, setIsApiKeyDraftVisible] = useState(false);

  useEffect(() => {
    if (!feedback) {
      return undefined;
    }

    const timeoutId = window.setTimeout(() => {
      setFeedback(null);
    }, 2800);

    return () => {
      window.clearTimeout(timeoutId);
    };
  }, [feedback]);

  const loadMicrophones = useCallback(async (showErrorFeedback: boolean) => {
    try {
      const devices = await invoke<MicrophoneInfo[]>("list_microphones");
      setMicrophones(devices);
    } catch (error) {
      if (showErrorFeedback) {
        setFeedback({
          kind: "error",
          message: toErrorMessage(error, "Unable to load microphones."),
        });
      }
    }
  }, []);

  const loadSettings = useCallback(async () => {
    setIsLoading(true);

    try {
      const [settings, hotkeyConfig, hasOpenAiKey] = await Promise.all([
        invoke<VoiceSettings>("get_settings"),
        invoke<HotkeyConfig>("get_hotkey_config"),
        invoke<boolean>("has_api_key", { provider: OPENAI_PROVIDER }),
      ]);

      setHotkeyShortcut(hotkeyConfig.shortcut || settings.hotkey_shortcut);
      setRecordingMode(
        normalizeRecordingMode(hotkeyConfig.mode || settings.recording_mode),
      );
      setMicrophoneId(settings.microphone_id ?? "");
      setLanguage(settings.language ?? "");
      setAutoInsert(settings.auto_insert);
      setLaunchAtLogin(settings.launch_at_login);
      setHasStoredApiKey(hasOpenAiKey);
      setApiKeyDraft("");
      setIsApiKeyDraftVisible(false);

      try {
        setLaunchAtLogin(await invoke<boolean>("get_launch_at_login"));
      } catch {
        // Fall back to persisted settings when the runtime autostart query fails.
      }

      await loadMicrophones(false);
    } catch (error) {
      setFeedback({
        kind: "error",
        message: toErrorMessage(error, "Unable to load settings."),
      });
    } finally {
      setIsLoading(false);
    }
  }, [loadMicrophones]);

  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  const selectedMicrophoneExists = useMemo(
    () => microphoneId === "" || microphones.some((device) => device.id === microphoneId),
    [microphoneId, microphones],
  );

  const apiKeyPlaceholder = hasStoredApiKey
    ? "Enter new key to replace existing key"
    : "sk-...";
  const canSaveApiKey = apiKeyDraft.trim().length > 0;
  const canClearApiKey = hasStoredApiKey;
  const canRevealApiKeyDraft = apiKeyDraft.trim().length > 0;

  async function handleSettingsSave(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setIsSavingSettings(true);

    try {
      const updatedSettings = await invoke<VoiceSettings>("apply_settings", {
        update: createSettingsUpdatePayload({
          hotkeyShortcut: normalizeShortcut(hotkeyShortcut),
          recordingMode,
          microphoneId,
          language,
          autoInsert,
          launchAtLogin,
        }),
      });

      setHotkeyShortcut(updatedSettings.hotkey_shortcut);
      setRecordingMode(normalizeRecordingMode(updatedSettings.recording_mode));
      setMicrophoneId(updatedSettings.microphone_id ?? "");
      setLanguage(updatedSettings.language ?? "");
      setAutoInsert(updatedSettings.auto_insert);
      setLaunchAtLogin(updatedSettings.launch_at_login);

      setFeedback({
        kind: "success",
        message: "Settings saved.",
      });
    } catch (error) {
      setFeedback({
        kind: "error",
        message: toErrorMessage(error, "Unable to save settings."),
      });
    } finally {
      setIsSavingSettings(false);
    }
  }

  async function handleRefreshMicrophones() {
    setIsRefreshingMics(true);
    await loadMicrophones(true);
    setIsRefreshingMics(false);
  }

  async function handleSaveApiKey() {
    const key = apiKeyDraft.trim();
    if (!key) {
      setFeedback({
        kind: "error",
        message: "Enter an API key before saving.",
      });
      return;
    }

    setIsSavingApiKey(true);

    try {
      await invoke("set_api_key", { provider: OPENAI_PROVIDER, key });
      setHasStoredApiKey(true);
      setApiKeyDraft("");
      setIsApiKeyDraftVisible(false);
      setFeedback({
        kind: "success",
        message: "OpenAI API key saved.",
      });
    } catch (error) {
      setFeedback({
        kind: "error",
        message: toErrorMessage(error, "Unable to save API key."),
      });
    } finally {
      setIsSavingApiKey(false);
    }
  }

  async function handleClearApiKey() {
    if (!hasStoredApiKey) {
      setApiKeyDraft("");
      setIsApiKeyDraftVisible(false);
      return;
    }

    setIsSavingApiKey(true);

    try {
      await invoke("delete_api_key", { provider: OPENAI_PROVIDER });
      setHasStoredApiKey(false);
      setApiKeyDraft("");
      setIsApiKeyDraftVisible(false);
      setFeedback({
        kind: "success",
        message: "OpenAI API key removed.",
      });
    } catch (error) {
      setFeedback({
        kind: "error",
        message: toErrorMessage(error, "Unable to clear API key."),
      });
    } finally {
      setIsSavingApiKey(false);
    }
  }

  async function handleExportLogs() {
    setIsExportingLogs(true);

    try {
      const logContents = await invoke<string>("export_logs");
      const timestamp = new Date().toISOString().replace(/[:.]/g, "-");
      const filename = `voice-logs-${timestamp}.log`;
      const blob = new Blob([logContents], {
        type: "text/plain;charset=utf-8",
      });
      const objectUrl = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = objectUrl;
      anchor.download = filename;
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(objectUrl);

      setFeedback({
        kind: "success",
        message: "Logs exported.",
      });
    } catch (error) {
      setFeedback({
        kind: "error",
        message: toErrorMessage(error, "Unable to export logs."),
      });
    } finally {
      setIsExportingLogs(false);
    }
  }

  return (
    <section className="settings-card" aria-live="polite">
      {isLoading ? (
        <p className="settings-loading">Loading settings...</p>
      ) : (
        <form className="settings-form" onSubmit={handleSettingsSave}>
          <label className="settings-field">
            <span className="field-label">Hotkey Shortcut</span>
            <input
              type="text"
              value={hotkeyShortcut}
              onChange={(event) => setHotkeyShortcut(event.currentTarget.value)}
              placeholder={normalizeShortcut("")}
              autoComplete="off"
              spellCheck={false}
            />
          </label>

          <div className="settings-field">
            <span className="field-label">Recording Mode</span>
            <div className="segmented-control" role="radiogroup" aria-label="Recording mode">
              <label
                className={`segment ${recordingMode === "hold_to_talk" ? "active" : ""}`}
              >
                <input
                  type="radio"
                  name="recordingMode"
                  value="hold_to_talk"
                  checked={recordingMode === "hold_to_talk"}
                  onChange={() => setRecordingMode("hold_to_talk")}
                />
                Hold-to-talk
              </label>
              <label className={`segment ${recordingMode === "toggle" ? "active" : ""}`}>
                <input
                  type="radio"
                  name="recordingMode"
                  value="toggle"
                  checked={recordingMode === "toggle"}
                  onChange={() => setRecordingMode("toggle")}
                />
                Toggle
              </label>
            </div>
          </div>

          <label className="settings-field">
            <span className="field-label">Microphone</span>
            <div className="field-row">
              <select
                value={microphoneId}
                onChange={(event) => setMicrophoneId(event.currentTarget.value)}
              >
                <option value="">System Default</option>
                {microphones.map((device) => (
                  <option key={device.id} value={device.id}>
                    {formatMicrophoneLabel(device)}
                  </option>
                ))}
                {!selectedMicrophoneExists && (
                  <option value={microphoneId}>
                    Previously selected device ({microphoneId})
                  </option>
                )}
              </select>
              <button
                type="button"
                className="secondary-button"
                onClick={handleRefreshMicrophones}
                disabled={isRefreshingMics}
              >
                {isRefreshingMics ? "Refreshing..." : "Refresh"}
              </button>
            </div>
          </label>

          <label className="settings-field">
            <span className="field-label">Language</span>
            <input
              type="text"
              value={language}
              onChange={(event) => setLanguage(event.currentTarget.value)}
              placeholder="Auto-detect"
              autoComplete="off"
              spellCheck={false}
            />
            <p className="field-description">
              Leave blank for auto-detection, or enter an ISO code (e.g. en, ja, fr).
            </p>
          </label>

          <hr className="settings-section-separator" />

          <label className="settings-field checkbox-field">
            <span className="field-label">Auto Insert</span>
            <input
              type="checkbox"
              checked={autoInsert}
              onChange={(event) => setAutoInsert(event.currentTarget.checked)}
            />
          </label>

          <label className="settings-field checkbox-field">
            <span className="field-label">Launch at Login</span>
            <input
              type="checkbox"
              checked={launchAtLogin}
              onChange={(event) => setLaunchAtLogin(event.currentTarget.checked)}
            />
          </label>

          <hr className="settings-section-separator" />

          <div className="settings-field">
            <span className="field-label">OpenAI API Key</span>
            <div className="field-row">
              <input
                type={isApiKeyDraftVisible ? "text" : "password"}
                value={apiKeyDraft}
                onChange={(event) => setApiKeyDraft(event.currentTarget.value)}
                placeholder={apiKeyPlaceholder}
                autoComplete="off"
                spellCheck={false}
              />
              <button
                type="button"
                className="secondary-button"
                onClick={() => setIsApiKeyDraftVisible((visible) => !visible)}
                disabled={!canRevealApiKeyDraft}
              >
                {isApiKeyDraftVisible ? "Hide" : "Reveal"}
              </button>
            </div>

            <div className="field-row">
              <button
                type="button"
                className="primary-button"
                onClick={handleSaveApiKey}
                disabled={!canSaveApiKey || isSavingApiKey}
              >
                {isSavingApiKey ? "Saving..." : "Save Key"}
              </button>
              <button
                type="button"
                className="secondary-button"
                onClick={handleClearApiKey}
                disabled={!canClearApiKey || isSavingApiKey}
              >
                Clear Key
              </button>
            </div>

            <p className="field-hint">
              {hasStoredApiKey
                ? "API key is set. Stored securely in macOS Keychain."
                : "No API key."}
            </p>
          </div>

          <div className="settings-actions">
            <button
              type="button"
              className="secondary-button"
              onClick={handleExportLogs}
              disabled={isExportingLogs}
            >
              {isExportingLogs ? "Exporting..." : "Export Logs"}
            </button>
            <button
              type="submit"
              className="primary-button"
              disabled={isSavingSettings}
            >
              {isSavingSettings ? "Saving..." : "Save Settings"}
            </button>
          </div>

          {feedback && (
            <p className={`settings-feedback settings-feedback-${feedback.kind}`}>
              {feedback.message}
            </p>
          )}
        </form>
      )}
    </section>
  );
}
