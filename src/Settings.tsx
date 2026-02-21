import { type FormEvent, useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Eye, EyeOff, RefreshCw, Download, Save, Key, Trash2 } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Separator } from "@/components/ui/separator";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";

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
    if (trimmed.length > 0) return trimmed;
  }
  if (error instanceof Error) {
    const trimmed = error.message.trim();
    if (trimmed.length > 0) return trimmed;
  }
  return fallback;
}

function formatMicrophoneLabel(device: MicrophoneInfo): string {
  const details: string[] = [];
  if (device.isDefault) details.push("Default");
  if (device.sampleRateHz) details.push(`${device.sampleRateHz} Hz`);
  if (device.channels) details.push(`${device.channels} ch`);
  if (details.length === 0) return device.name;
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
    if (!feedback) return undefined;
    const timeoutId = window.setTimeout(() => setFeedback(null), 2800);
    return () => window.clearTimeout(timeoutId);
  }, [feedback]);

  const loadMicrophones = useCallback(async (showErrorFeedback: boolean) => {
    try {
      const devices = await invoke<MicrophoneInfo[]>("list_microphones");
      setMicrophones(devices);
    } catch (error) {
      if (showErrorFeedback) {
        setFeedback({ kind: "error", message: toErrorMessage(error, "Unable to load microphones.") });
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
      setRecordingMode(normalizeRecordingMode(hotkeyConfig.mode || settings.recording_mode));
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
        // Fall back to persisted settings
      }

      await loadMicrophones(false);
    } catch (error) {
      setFeedback({ kind: "error", message: toErrorMessage(error, "Unable to load settings.") });
    } finally {
      setIsLoading(false);
    }
  }, [loadMicrophones]);

  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  const selectedMicrophoneExists = useMemo(
    () => microphoneId === "" || microphones.some((device) => device.id === microphoneId),
    [microphoneId, microphones]
  );

  const apiKeyPlaceholder = hasStoredApiKey ? "Enter new key to replace existing key" : "sk-...";
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
      setFeedback({ kind: "success", message: "Settings saved." });
    } catch (error) {
      setFeedback({ kind: "error", message: toErrorMessage(error, "Unable to save settings.") });
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
      setFeedback({ kind: "error", message: "Enter an API key before saving." });
      return;
    }

    setIsSavingApiKey(true);
    try {
      await invoke("set_api_key", { provider: OPENAI_PROVIDER, key });
      setHasStoredApiKey(true);
      setApiKeyDraft("");
      setIsApiKeyDraftVisible(false);
      setFeedback({ kind: "success", message: "OpenAI API key saved." });
    } catch (error) {
      setFeedback({ kind: "error", message: toErrorMessage(error, "Unable to save API key.") });
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
      setFeedback({ kind: "success", message: "OpenAI API key removed." });
    } catch (error) {
      setFeedback({ kind: "error", message: toErrorMessage(error, "Unable to clear API key.") });
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
      const blob = new Blob([logContents], { type: "text/plain;charset=utf-8" });
      const objectUrl = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = objectUrl;
      anchor.download = filename;
      document.body.append(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(objectUrl);
      setFeedback({ kind: "success", message: "Logs exported." });
    } catch (error) {
      setFeedback({ kind: "error", message: toErrorMessage(error, "Unable to export logs.") });
    } finally {
      setIsExportingLogs(false);
    }
  }

  if (isLoading) {
    return (
      <Card>
        <CardContent className="py-8 text-center">
          <p className="text-sm text-muted-foreground">Loading settings...</p>
        </CardContent>
      </Card>
    );
  }

  return (
    <form className="space-y-4" onSubmit={handleSettingsSave} aria-live="polite">
      {/* ── Recording ── */}
      <Card>
        <CardContent className="space-y-4 py-4">
          <p className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            Recording
          </p>

          {/* Hotkey */}
          <div className="space-y-1.5">
            <Label htmlFor="hotkey" className="text-xs">
              Hotkey Shortcut
            </Label>
            <Input
              id="hotkey"
              value={hotkeyShortcut}
              onChange={(e) => setHotkeyShortcut(e.currentTarget.value)}
              placeholder={normalizeShortcut("")}
              autoComplete="off"
              spellCheck={false}
              className="h-8 text-xs"
            />
          </div>

          {/* Recording Mode */}
          <div className="space-y-1.5">
            <Label className="text-xs">Recording Mode</Label>
            <div className="flex rounded-lg border bg-muted/50 p-0.5">
              {(["hold_to_talk", "toggle"] as const).map((mode) => (
                <button
                  key={mode}
                  type="button"
                  className={cn(
                    "flex-1 rounded-md px-3 py-1.5 text-xs font-medium transition-all",
                    recordingMode === mode
                      ? "bg-background text-foreground shadow-sm"
                      : "text-muted-foreground hover:text-foreground"
                  )}
                  onClick={() => setRecordingMode(mode)}
                >
                  {mode === "hold_to_talk" ? "Hold-to-talk" : "Toggle"}
                </button>
              ))}
            </div>
          </div>

          {/* Microphone */}
          <div className="space-y-1.5">
            <Label className="text-xs">Microphone</Label>
            <div className="flex gap-2">
              <Select value={microphoneId} onValueChange={setMicrophoneId}>
                <SelectTrigger className="h-8 flex-1 text-xs">
                  <SelectValue placeholder="System Default" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="">System Default</SelectItem>
                  {microphones.map((device) => (
                    <SelectItem key={device.id} value={device.id}>
                      {formatMicrophoneLabel(device)}
                    </SelectItem>
                  ))}
                  {!selectedMicrophoneExists && microphoneId && (
                    <SelectItem value={microphoneId}>
                      Previously selected ({microphoneId})
                    </SelectItem>
                  )}
                </SelectContent>
              </Select>
              <Button
                type="button"
                variant="outline"
                size="icon-sm"
                onClick={handleRefreshMicrophones}
                disabled={isRefreshingMics}
              >
                <RefreshCw className={cn("size-3.5", isRefreshingMics && "animate-spin")} />
              </Button>
            </div>
          </div>

          {/* Language */}
          <div className="space-y-1.5">
            <Label htmlFor="language" className="text-xs">
              Language
            </Label>
            <Input
              id="language"
              value={language}
              onChange={(e) => setLanguage(e.currentTarget.value)}
              placeholder="Auto-detect"
              autoComplete="off"
              spellCheck={false}
              className="h-8 text-xs"
            />
            <p className="text-[11px] text-muted-foreground">
              Leave blank for auto-detection, or enter an ISO code (e.g. en, ja, fr).
            </p>
          </div>
        </CardContent>
      </Card>

      {/* ── Behavior ── */}
      <Card>
        <CardContent className="space-y-4 py-4">
          <p className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            Behavior
          </p>

          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <Label htmlFor="auto-insert" className="text-xs font-medium">
                Auto Insert
              </Label>
              <p className="text-[11px] text-muted-foreground">
                Paste transcript into focused app
              </p>
            </div>
            <Switch
              id="auto-insert"
              checked={autoInsert}
              onCheckedChange={setAutoInsert}
            />
          </div>

          <Separator />

          <div className="flex items-center justify-between">
            <div className="space-y-0.5">
              <Label htmlFor="launch-login" className="text-xs font-medium">
                Launch at Login
              </Label>
              <p className="text-[11px] text-muted-foreground">
                Start Voice when you log in
              </p>
            </div>
            <Switch
              id="launch-login"
              checked={launchAtLogin}
              onCheckedChange={setLaunchAtLogin}
            />
          </div>
        </CardContent>
      </Card>

      {/* ── API Key ── */}
      <Card>
        <CardContent className="space-y-3 py-4">
          <div className="flex items-center gap-2">
            <Key className="size-3.5 text-muted-foreground" />
            <p className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
              OpenAI API Key
            </p>
          </div>

          <div className="flex gap-2">
            <Input
              type={isApiKeyDraftVisible ? "text" : "password"}
              value={apiKeyDraft}
              onChange={(e) => setApiKeyDraft(e.currentTarget.value)}
              placeholder={apiKeyPlaceholder}
              autoComplete="off"
              spellCheck={false}
              className="h-8 flex-1 text-xs font-mono"
            />
            <Button
              type="button"
              variant="outline"
              size="icon-sm"
              onClick={() => setIsApiKeyDraftVisible((v) => !v)}
              disabled={!canRevealApiKeyDraft}
            >
              {isApiKeyDraftVisible ? <EyeOff className="size-3.5" /> : <Eye className="size-3.5" />}
            </Button>
          </div>

          <div className="flex gap-2">
            <Button
              type="button"
              size="xs"
              onClick={handleSaveApiKey}
              disabled={!canSaveApiKey || isSavingApiKey}
            >
              {isSavingApiKey ? "Saving..." : "Save Key"}
            </Button>
            <Button
              type="button"
              variant="outline"
              size="xs"
              onClick={handleClearApiKey}
              disabled={!canClearApiKey || isSavingApiKey}
            >
              <Trash2 className="size-3" />
              Clear Key
            </Button>
          </div>

          <p className="text-[11px] text-muted-foreground">
            {hasStoredApiKey
              ? "✓ API key set. Stored securely in macOS Keychain."
              : "No API key configured."}
          </p>
        </CardContent>
      </Card>

      {/* ── Actions ── */}
      <div className="flex items-center justify-between">
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={handleExportLogs}
          disabled={isExportingLogs}
        >
          <Download className="size-3.5" />
          {isExportingLogs ? "Exporting..." : "Export Logs"}
        </Button>

        <Button type="submit" size="sm" disabled={isSavingSettings}>
          <Save className="size-3.5" />
          {isSavingSettings ? "Saving..." : "Save Settings"}
        </Button>
      </div>

      {/* Feedback */}
      {feedback && (
        <Alert
          variant={feedback.kind === "error" ? "destructive" : "default"}
          className={cn(
            "py-2",
            feedback.kind === "success" &&
              "border-emerald-500/30 bg-emerald-50/50 dark:bg-emerald-950/20"
          )}
        >
          <AlertDescription
            className={cn(
              "text-xs",
              feedback.kind === "success" && "text-emerald-700 dark:text-emerald-400"
            )}
          >
            {feedback.message}
          </AlertDescription>
        </Alert>
      )}
    </form>
  );
}
