import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { CheckCircle2, Circle, Loader2, Mic, Shield, Sparkles } from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { cn } from "@/lib/utils";
import {
  DEFAULT_HOTKEY_SHORTCUT,
  normalizeRecordingMode,
  OPENAI_PROVIDER,
  shortcutFromKeyboardEvent,
  type RecordingMode,
} from "./settingsUtils";

type PermissionState = "not_determined" | "granted" | "denied";
type PermissionSnapshot = {
  microphone: PermissionState;
  accessibility: PermissionState;
  allGranted: boolean;
};
type ChatGptAuthStatus = {
  accountId: string;
  expiresAt: number;
};
type VoiceSettings = {
  hotkey_shortcut: string;
  recording_mode: string;
};
type VoiceSettingsUpdate = {
  hotkey_shortcut?: string;
  recording_mode?: RecordingMode;
};
type AuthMethod = "oauth" | "api_key";

type OnboardingProps = {
  onComplete: () => void;
};

const TOTAL_STEPS = 6;
const STEP_TITLES = [
  "Welcome",
  "Microphone",
  "Accessibility",
  "Authentication",
  "Shortcut + Mode",
  "All Done",
] as const;

const HOTKEY_PRESETS = [
  { label: "Alt + Space", value: "Alt+Space" },
  { label: "Ctrl + Space", value: "Ctrl+Space" },
  { label: "Shift + Space", value: "Shift+Space" },
  { label: "Cmd + Space", value: "Cmd+Space" },
] as const;

const CUSTOM_SHORTCUT_PRESET_VALUE = "__custom_shortcut__";

function toErrorMessage(error: unknown, fallback: string): string {
  if (typeof error === "string" && error.trim().length > 0) return error;
  if (error instanceof Error && error.message.trim().length > 0) return error.message;
  return fallback;
}

function keyDisplayLabel(key: string): string {
  const normalized = key.trim().toLowerCase();
  if (normalized === "meta" || normalized === "cmd" || normalized === "command") return "Cmd";
  if (normalized === "control" || normalized === "ctrl") return "Ctrl";
  if (normalized === "option") return "Alt";
  if (normalized === "spacebar") return "Space";
  return key.trim();
}

function splitShortcut(shortcut: string): string[] {
  return shortcut
    .split("+")
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
}

function stopInstruction(mode: RecordingMode): string {
  return mode === "hold_to_talk" ? "Release to stop." : "Press again to stop.";
}

function normalizeOnboardingRecordingMode(value: string): RecordingMode {
  const normalized = normalizeRecordingMode(value);
  return normalized === "double_tap_toggle" ? "toggle" : normalized;
}

function ShortcutKeycaps({
  shortcut,
  large = false,
}: {
  shortcut: string;
  large?: boolean;
}) {
  const parts = splitShortcut(shortcut);
  if (parts.length === 0) {
    return <span className="text-xs text-muted-foreground">No shortcut set</span>;
  }

  return (
    <div className="flex flex-wrap items-center justify-center gap-2">
      {parts.map((part, index) => (
        <kbd
          key={`${part}-${index}`}
          className={cn(
            "inline-flex items-center justify-center rounded-md border border-border/80 bg-background font-mono font-medium text-foreground shadow-[0_1px_0_1px_hsl(var(--border)/0.45)]",
            large ? "min-w-[52px] px-3 py-1.5 text-sm" : "min-w-[30px] px-2 py-0.5 text-xs"
          )}
        >
          {keyDisplayLabel(part)}
        </kbd>
      ))}
    </div>
  );
}

export default function Onboarding({ onComplete }: OnboardingProps) {
  const [step, setStep] = useState(0);
  const [permissions, setPermissions] = useState<PermissionSnapshot | null>(null);
  const [hasApiKey, setHasApiKey] = useState(false);
  const [chatgptAuthStatus, setChatgptAuthStatus] = useState<ChatGptAuthStatus | null>(null);
  const [selectedAuthMethod, setSelectedAuthMethod] = useState<AuthMethod>("oauth");
  const [apiKeyDraft, setApiKeyDraft] = useState("");
  const [hotkeyShortcut, setHotkeyShortcut] = useState(DEFAULT_HOTKEY_SHORTCUT);
  const [recordingMode, setRecordingMode] = useState<RecordingMode>("toggle");
  const [isRecordingShortcut, setIsRecordingShortcut] = useState(false);
  const [errorMessage, setErrorMessage] = useState("");
  const [isLoadingInitialState, setIsLoadingInitialState] = useState(true);
  const [isRequestingMic, setIsRequestingMic] = useState(false);
  const [isOpeningAccessibilitySettings, setIsOpeningAccessibilitySettings] = useState(false);
  const [isStartingOauth, setIsStartingOauth] = useState(false);
  const [isSavingApiKey, setIsSavingApiKey] = useState(false);
  const [isSavingShortcutSettings, setIsSavingShortcutSettings] = useState(false);
  const [isCompleting, setIsCompleting] = useState(false);

  const micGranted = permissions?.microphone === "granted";
  const accessibilityGranted = permissions?.accessibility === "granted";
  const authConfigured = hasApiKey || chatgptAuthStatus !== null;

  const selectedShortcutPreset = useMemo(() => {
    const normalized = hotkeyShortcut.trim().toLowerCase();
    const matchingPreset = HOTKEY_PRESETS.find(
      (preset) => preset.value.toLowerCase() === normalized,
    );
    return matchingPreset?.value ?? CUSTOM_SHORTCUT_PRESET_VALUE;
  }, [hotkeyShortcut]);
  const shortcutInstructionLabel = useMemo(() => {
    const formatted = splitShortcut(hotkeyShortcut).map(keyDisplayLabel).join(" + ");
    if (formatted.length > 0) {
      return formatted;
    }

    return splitShortcut(DEFAULT_HOTKEY_SHORTCUT).map(keyDisplayLabel).join(" + ");
  }, [hotkeyShortcut]);

  const authSuccessMessage = useMemo(() => {
    if (chatgptAuthStatus) {
      return `ChatGPT connected (${chatgptAuthStatus.accountId}).`;
    }
    if (hasApiKey) {
      return "OpenAI API key saved.";
    }
    return "";
  }, [chatgptAuthStatus, hasApiKey]);

  const refreshPermissionStatus = useCallback(async () => {
    try {
      const snapshot = await invoke<PermissionSnapshot>("check_permissions");
      setPermissions(snapshot);
    } catch {
      // Ignore transient polling errors
    }
  }, []);

  const loadInitialState = useCallback(async () => {
    setIsLoadingInitialState(true);
    try {
      const [snapshot, apiKeyPresent, authStatus, settings] = await Promise.all([
        invoke<PermissionSnapshot>("check_permissions"),
        invoke<boolean>("has_api_key", { provider: OPENAI_PROVIDER }),
        invoke<ChatGptAuthStatus | null>("get_auth_status"),
        invoke<VoiceSettings>("get_settings"),
      ]);

      setPermissions(snapshot);
      setHasApiKey(apiKeyPresent);
      setChatgptAuthStatus(authStatus);
      setHotkeyShortcut(settings.hotkey_shortcut || DEFAULT_HOTKEY_SHORTCUT);
      setRecordingMode(normalizeOnboardingRecordingMode(settings.recording_mode));
      setErrorMessage("");
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "Unable to load onboarding state."));
    } finally {
      setIsLoadingInitialState(false);
    }
  }, []);

  useEffect(() => {
    void loadInitialState();
  }, [loadInitialState]);

  useEffect(() => {
    if (step !== 1 && step !== 2) return undefined;
    void refreshPermissionStatus();
    const interval = window.setInterval(() => {
      void refreshPermissionStatus();
    }, 1200);
    return () => window.clearInterval(interval);
  }, [refreshPermissionStatus, step]);

  useEffect(() => {
    const handleFocus = () => {
      void refreshPermissionStatus();
    };
    window.addEventListener("focus", handleFocus);
    return () => window.removeEventListener("focus", handleFocus);
  }, [refreshPermissionStatus]);

  useEffect(() => {
    if (step !== 3 || !authConfigured) return undefined;

    const timeoutId = window.setTimeout(() => {
      setStep((current) => (current === 3 ? 4 : current));
    }, 450);

    return () => window.clearTimeout(timeoutId);
  }, [authConfigured, step]);

  useEffect(() => {
    if (!isRecordingShortcut || step !== 4) {
      return undefined;
    }

    const handleShortcutKeydown = (event: KeyboardEvent) => {
      event.preventDefault();
      event.stopPropagation();

      if (event.repeat) {
        return;
      }

      const capturedShortcut = shortcutFromKeyboardEvent(event);
      if (!capturedShortcut) {
        return;
      }

      setIsRecordingShortcut(false);
      setHotkeyShortcut(capturedShortcut);
    };

    window.addEventListener("keydown", handleShortcutKeydown, true);
    return () => {
      window.removeEventListener("keydown", handleShortcutKeydown, true);
    };
  }, [isRecordingShortcut, step]);

  const handleRequestMic = useCallback(async () => {
    setIsRequestingMic(true);
    setErrorMessage("");
    try {
      const snapshot = await invoke<PermissionSnapshot>("request_mic_permission");
      setPermissions(snapshot);
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "Unable to request microphone access."));
    } finally {
      setIsRequestingMic(false);
    }
  }, []);

  const handleOpenAccessibilitySettings = useCallback(async () => {
    setIsOpeningAccessibilitySettings(true);
    setErrorMessage("");
    try {
      await invoke("open_accessibility_settings");
      void refreshPermissionStatus();
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "Unable to open Accessibility settings."));
    } finally {
      setIsOpeningAccessibilitySettings(false);
    }
  }, [refreshPermissionStatus]);

  const handleStartOauth = useCallback(async () => {
    setSelectedAuthMethod("oauth");
    setIsStartingOauth(true);
    setErrorMessage("");
    try {
      const status = await invoke<ChatGptAuthStatus>("start_oauth_login");
      setChatgptAuthStatus(status);
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "ChatGPT login failed."));
    } finally {
      setIsStartingOauth(false);
    }
  }, []);

  const handleSaveApiKey = useCallback(async () => {
    const key = apiKeyDraft.trim();
    if (!key) {
      setErrorMessage("Paste an API key before saving.");
      return;
    }

    setSelectedAuthMethod("api_key");
    setIsSavingApiKey(true);
    setErrorMessage("");
    try {
      await invoke("save_api_key", { provider: OPENAI_PROVIDER, key });
      setHasApiKey(true);
      setApiKeyDraft("");
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "Unable to save API key."));
    } finally {
      setIsSavingApiKey(false);
    }
  }, [apiKeyDraft]);

  const handleSaveShortcutAndMode = useCallback(async () => {
    setIsSavingShortcutSettings(true);
    setErrorMessage("");

    try {
      const updated = await invoke<VoiceSettings>("apply_settings", {
        update: {
          hotkey_shortcut: hotkeyShortcut.trim() || DEFAULT_HOTKEY_SHORTCUT,
          recording_mode: recordingMode,
        } as VoiceSettingsUpdate,
      });

      setHotkeyShortcut(updated.hotkey_shortcut || DEFAULT_HOTKEY_SHORTCUT);
      setRecordingMode(normalizeOnboardingRecordingMode(updated.recording_mode));
      setStep(5);
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "Unable to save recording controls."));
    } finally {
      setIsSavingShortcutSettings(false);
    }
  }, [hotkeyShortcut, recordingMode]);

  const handleCompleteOnboarding = useCallback(async () => {
    setIsCompleting(true);
    setErrorMessage("");
    try {
      await invoke<boolean>("complete_onboarding");
      onComplete();
    } catch (error) {
      setErrorMessage(toErrorMessage(error, "Unable to complete onboarding."));
    } finally {
      setIsCompleting(false);
    }
  }, [onComplete]);

  const renderStep = () => {
    if (step === 0) {
      return (
        <div className="space-y-6 text-center">
          <div className="space-y-2">
            <h2 className="text-2xl font-semibold tracking-tight">Welcome to Buzz 🐝</h2>
            <p className="text-sm text-muted-foreground">
              Voice-to-text with a quick buzz. Let&apos;s get your setup dialed in.
            </p>
          </div>
          <Button onClick={() => setStep(1)} size="sm">
            Get Started
          </Button>
        </div>
      );
    }

    if (step === 1) {
      return (
        <div className="space-y-5">
          <div className="space-y-2">
            <h2 className="text-xl font-semibold tracking-tight">Allow microphone access</h2>
            <p className="text-sm text-muted-foreground">
              Buzz needs microphone access to capture your voice and transcribe it to text.
            </p>
          </div>

          <div className="flex items-center gap-2 rounded-lg border bg-muted/30 px-3 py-2 text-sm">
            {micGranted ? (
              <CheckCircle2 className="size-4 text-emerald-500" />
            ) : (
              <Circle className="size-4 text-muted-foreground" />
            )}
            <span>{micGranted ? "Microphone permission granted" : "Waiting for microphone access"}</span>
          </div>

          <div className="flex items-center justify-between">
            <Button variant="outline" onClick={handleRequestMic} disabled={isRequestingMic}>
              {isRequestingMic ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Requesting...
                </>
              ) : (
                <>
                  <Mic className="size-4" />
                  Grant Microphone Access
                </>
              )}
            </Button>
            <Button onClick={() => setStep(2)} disabled={!micGranted}>
              Continue
            </Button>
          </div>
        </div>
      );
    }

    if (step === 2) {
      return (
        <div className="space-y-5">
          <div className="space-y-2">
            <h2 className="text-xl font-semibold tracking-tight">Allow accessibility access</h2>
            <p className="text-sm text-muted-foreground">
              Buzz uses Accessibility to insert transcribed text at your cursor in other apps.
            </p>
          </div>

          <div className="flex items-center gap-2 rounded-lg border bg-muted/30 px-3 py-2 text-sm">
            {accessibilityGranted ? (
              <CheckCircle2 className="size-4 text-emerald-500" />
            ) : (
              <Circle className="size-4 text-muted-foreground" />
            )}
            <span>
              {accessibilityGranted
                ? "Accessibility permission granted"
                : "Waiting for accessibility access"}
            </span>
          </div>

          <p className="text-xs text-muted-foreground">
            macOS requires you to manually enable Buzz in System Settings after opening the panel.
          </p>

          <div className="flex items-center justify-between">
            <Button
              variant="outline"
              onClick={handleOpenAccessibilitySettings}
              disabled={isOpeningAccessibilitySettings}
            >
              {isOpeningAccessibilitySettings ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Opening...
                </>
              ) : (
                <>
                  <Shield className="size-4" />
                  Open System Settings
                </>
              )}
            </Button>
            <Button onClick={() => setStep(3)} disabled={!accessibilityGranted}>
              Continue
            </Button>
          </div>
        </div>
      );
    }

    if (step === 3) {
      return (
        <div className="space-y-5">
          <div className="space-y-2">
            <h2 className="text-xl font-semibold tracking-tight">Choose authentication</h2>
            <p className="text-sm text-muted-foreground">
              Click a card to continue with ChatGPT OAuth or API key authentication.
            </p>
          </div>

          <div className="grid gap-3 md:grid-cols-2">
            <button
              type="button"
              className="text-left"
              onClick={() => {
                void handleStartOauth();
              }}
              disabled={isStartingOauth}
            >
              <Card
                className={cn(
                  "h-full border transition-all hover:border-primary/60",
                  selectedAuthMethod === "oauth" ? "border-primary bg-primary/5" : "border-border"
                )}
              >
                <CardHeader className="pb-2">
                  <CardTitle className="text-sm">Sign in with ChatGPT</CardTitle>
                  <CardDescription>
                    Use your ChatGPT account. This opens your browser for OAuth login.
                  </CardDescription>
                </CardHeader>
                <CardContent className="pt-0 text-xs text-muted-foreground">
                  {isStartingOauth && selectedAuthMethod === "oauth" ? (
                    <span className="inline-flex items-center gap-2 text-primary">
                      <Loader2 className="size-3.5 animate-spin" />
                      Opening browser...
                    </span>
                  ) : (
                    "Click to continue with ChatGPT"
                  )}
                </CardContent>
              </Card>
            </button>

            <button
              type="button"
              className="text-left"
              onClick={() => {
                setSelectedAuthMethod("api_key");
                setErrorMessage("");
              }}
            >
              <Card
                className={cn(
                  "h-full border transition-all hover:border-primary/60",
                  selectedAuthMethod === "api_key" ? "border-primary bg-primary/5" : "border-border"
                )}
              >
                <CardHeader className="pb-2">
                  <CardTitle className="text-sm">Use API Key</CardTitle>
                  <CardDescription>
                    Paste an OpenAI key directly if you prefer key-based auth.
                  </CardDescription>
                </CardHeader>
                <CardContent className="pt-0 text-xs text-muted-foreground">
                  Click to enter your API key
                </CardContent>
              </Card>
            </button>
          </div>

          {selectedAuthMethod === "api_key" && !hasApiKey && (
            <div className="space-y-2 rounded-lg border bg-muted/30 p-3">
              <Input
                value={apiKeyDraft}
                onChange={(event) => setApiKeyDraft(event.currentTarget.value)}
                placeholder="sk-..."
                className="h-8 text-xs font-mono"
                autoComplete="off"
              />
              <Button
                size="sm"
                className="w-full"
                onClick={handleSaveApiKey}
                disabled={isSavingApiKey}
              >
                {isSavingApiKey ? (
                  <>
                    <Loader2 className="size-4 animate-spin" />
                    Saving...
                  </>
                ) : (
                  "Save API Key"
                )}
              </Button>
            </div>
          )}

          {authConfigured && (
            <Alert className="border-emerald-500/30 bg-emerald-50/60 dark:bg-emerald-950/20">
              <AlertDescription className="flex items-center gap-2 text-emerald-700 dark:text-emerald-300">
                <CheckCircle2 className="size-4" />
                {authSuccessMessage} Moving to shortcut setup...
              </AlertDescription>
            </Alert>
          )}
        </div>
      );
    }

    if (step === 4) {
      return (
        <div className="space-y-6">
          <div className="space-y-2">
            <h2 className="text-xl font-semibold tracking-tight">Set your recording controls</h2>
            <p className="text-sm text-muted-foreground">
              Choose your mode and shortcut. You can change these anytime in Settings.
            </p>
          </div>

          <div className="space-y-3">
            <p className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
              Recording mode
            </p>
            <div className="grid gap-3 md:grid-cols-2">
              <button
                type="button"
                className="text-left"
                onClick={() => setRecordingMode("toggle")}
              >
                <Card
                  className={cn(
                    "h-full border transition-all hover:border-primary/60",
                    recordingMode === "toggle" ? "border-primary bg-primary/5" : "border-border"
                  )}
                >
                  <CardHeader className="pb-2">
                    <CardTitle className="text-base">Toggle</CardTitle>
                    <CardDescription>
                      Press once to start, press again to stop.
                    </CardDescription>
                  </CardHeader>
                </Card>
              </button>

              <button
                type="button"
                className="text-left"
                onClick={() => setRecordingMode("hold_to_talk")}
              >
                <Card
                  className={cn(
                    "h-full border transition-all hover:border-primary/60",
                    recordingMode === "hold_to_talk" ? "border-primary bg-primary/5" : "border-border"
                  )}
                >
                  <CardHeader className="pb-2">
                    <CardTitle className="text-base">Hold to Talk</CardTitle>
                    <CardDescription>
                      Hold the key while speaking, release to stop.
                    </CardDescription>
                  </CardHeader>
                </Card>
              </button>
            </div>
          </div>

          <div className="space-y-3">
            <p className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
              Recording shortcut
            </p>

            <div className="grid gap-2 sm:grid-cols-2">
              {HOTKEY_PRESETS.map((preset) => (
                <Button
                  key={preset.value}
                  type="button"
                  variant={selectedShortcutPreset === preset.value ? "default" : "outline"}
                  className="justify-between"
                  onClick={() => {
                    setIsRecordingShortcut(false);
                    setHotkeyShortcut(preset.value);
                  }}
                >
                  <span>{preset.label}</span>
                </Button>
              ))}
            </div>

            <Button
              type="button"
              variant={isRecordingShortcut || selectedShortcutPreset === CUSTOM_SHORTCUT_PRESET_VALUE ? "default" : "outline"}
              className="w-full"
              onClick={() => setIsRecordingShortcut((active) => !active)}
            >
              {isRecordingShortcut ? "Cancel Custom Shortcut" : "Record Custom Shortcut"}
            </Button>

            {isRecordingShortcut && (
              <p className="text-xs text-primary">
                Press the key combination you want to use...
              </p>
            )}
          </div>

          <div className="space-y-3 rounded-xl border-2 border-primary/35 bg-primary/5 p-4 text-center">
            <p className="text-base font-semibold text-primary">
              Press {shortcutInstructionLabel} to start recording
            </p>
            <ShortcutKeycaps shortcut={hotkeyShortcut} large />
            <p className="text-sm text-muted-foreground">{stopInstruction(recordingMode)}</p>
          </div>

          <div className="flex justify-end">
            <Button onClick={handleSaveShortcutAndMode} disabled={isSavingShortcutSettings}>
              {isSavingShortcutSettings ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Saving...
                </>
              ) : (
                "Continue"
              )}
            </Button>
          </div>
        </div>
      );
    }

    const modeLabel = recordingMode === "hold_to_talk" ? "Hold to Talk" : "Toggle";

    return (
      <div className="space-y-6">
        <div className="space-y-2 text-center">
          <h2 className="text-2xl font-semibold tracking-tight">You&apos;re all set! 🐝</h2>
          <p className="text-sm text-muted-foreground">Here&apos;s how to start recording with Buzz.</p>
        </div>

        <div className="space-y-3 rounded-xl border bg-muted/30 p-4">
          <div className="space-y-2 text-center">
            <p className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
              Shortcut
            </p>
            <ShortcutKeycaps shortcut={hotkeyShortcut} large />
          </div>

          <div className="flex items-center justify-center">
            <span className="rounded-full border bg-background px-3 py-1 text-xs font-medium">
              Mode: {modeLabel}
            </span>
          </div>

          <p className="text-center text-sm text-muted-foreground">
            Press {shortcutInstructionLabel} to start recording. {stopInstruction(recordingMode)}
          </p>
        </div>

        <div className="flex justify-center">
          <Button onClick={handleCompleteOnboarding} disabled={isCompleting}>
            {isCompleting ? (
              <>
                <Loader2 className="size-4 animate-spin" />
                Finalizing...
              </>
            ) : (
              "Start Using Buzz"
            )}
          </Button>
        </div>
      </div>
    );
  };

  return (
    <main className="flex min-h-screen items-center justify-center bg-gradient-to-b from-background to-muted/30 px-4 py-8">
      <Card className="w-full max-w-2xl border-border/70 shadow-sm">
        <CardHeader className="space-y-4 pb-1">
          <div className="flex items-center justify-center gap-2 text-xs text-muted-foreground">
            <Sparkles className="size-3.5" />
            First-run setup
          </div>

          <div className="space-y-2">
            <div className="flex items-center justify-center gap-2">
              {STEP_TITLES.map((title, index) => (
                <div key={title} className="flex items-center gap-2">
                  <div
                    className={cn(
                      "size-2.5 rounded-full transition-colors",
                      index <= step ? "bg-primary" : "bg-muted"
                    )}
                  />
                  {index < STEP_TITLES.length - 1 && (
                    <div
                      className={cn(
                        "h-px w-5 transition-colors",
                        index < step ? "bg-primary/70" : "bg-border"
                      )}
                    />
                  )}
                </div>
              ))}
            </div>
            <p className="text-center text-xs text-muted-foreground">
              Step {step + 1} of {TOTAL_STEPS}: {STEP_TITLES[step]}
            </p>
          </div>
        </CardHeader>

        <CardContent className="py-6">
          {isLoadingInitialState ? (
            <div className="flex items-center justify-center gap-2 py-12 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              Loading onboarding...
            </div>
          ) : (
            <div key={step} className="animate-in fade-in-0 slide-in-from-bottom-1 duration-300">
              {renderStep()}
            </div>
          )}

          {errorMessage && (
            <Alert variant="destructive" className="mt-5 py-2">
              <AlertDescription className="text-xs">{errorMessage}</AlertDescription>
            </Alert>
          )}
        </CardContent>
      </Card>
    </main>
  );
}
