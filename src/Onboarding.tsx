import { useCallback, useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { CheckCircle2, ChevronLeft, Circle, Loader2, Mic, Shield, Sparkles } from "lucide-react";
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
import {
  onboardingAuthSuccessMessage,
  extractTranscriptText,
  practiceStatusLabel,
  shouldShowOnboardingApiKeyInput,
  type OnboardingAuthMethod,
  type OnboardingPracticeStatus,
} from "./onboardingUtils";

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
type TranscriptReadyEvent = { text?: string };
type PipelineErrorEvent = { stage: string; message: string };

type OnboardingProps = {
  onComplete: () => void;
};

const TOTAL_STEPS = 7;
const STEP_TITLES = [
  "Welcome",
  "Microphone",
  "Accessibility",
  "Authentication",
  "Shortcut + Mode",
  "Try It Out",
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
  return normalizeRecordingMode(value);
}

/* ─── Keycap badges ─────────────────────────────────── */

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
    <div className="flex flex-wrap items-center justify-center gap-1.5">
      {parts.map((part, index) => (
        <span key={`${part}-${index}`} className="flex items-center gap-1.5">
          <kbd
            className={cn(
              "inline-flex items-center justify-center rounded-lg border font-mono font-semibold",
              "border-border bg-background text-foreground shadow-sm",
              large
                ? "min-w-[52px] px-3.5 py-2 text-sm tracking-wide"
                : "min-w-[30px] px-2 py-0.5 text-xs",
            )}
          >
            {keyDisplayLabel(part)}
          </kbd>
          {index < parts.length - 1 && (
            <span className={cn("font-medium text-muted-foreground/50", large ? "text-sm" : "text-[10px]")}>
              +
            </span>
          )}
        </span>
      ))}
    </div>
  );
}

/* ─── Selectable card wrapper ───────────────────────── */

function SelectableCard({
  selected,
  onClick,
  disabled,
  children,
  className,
}: {
  selected: boolean;
  onClick: () => void;
  disabled?: boolean;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <button
      type="button"
      className={cn("text-left w-full", className)}
      onClick={onClick}
      disabled={disabled}
    >
      <Card
        className={cn(
          "h-full border transition-all duration-200 cursor-pointer",
          selected
            ? "border-primary/50 bg-accent/40 ring-1 ring-primary/20"
            : "border-border hover:border-primary/30 hover:bg-accent/20",
        )}
      >
        {children}
      </Card>
    </button>
  );
}

/* ─── Progress dots ─────────────────────────────────── */

function ProgressDots({ currentStep }: { currentStep: number }) {
  return (
    <div className="space-y-3">
      <div className="flex items-center justify-center gap-1.5">
        {STEP_TITLES.map((title, index) => (
          <div key={title} className="flex items-center gap-1.5">
            <div
              className={cn(
                "size-2 rounded-full transition-all duration-300",
                index < currentStep && "bg-primary/70",
                index === currentStep && "bg-primary ring-[2px] ring-primary/20",
                index > currentStep && "bg-muted",
              )}
            />
            {index < STEP_TITLES.length - 1 && (
              <div
                className={cn(
                  "h-px w-5 transition-all duration-300",
                  index < currentStep ? "bg-primary/40" : "bg-border",
                )}
              />
            )}
          </div>
        ))}
      </div>
      <p className="text-center text-[11px] font-medium text-muted-foreground/70">
        Step {currentStep + 1} of {TOTAL_STEPS} &middot; {STEP_TITLES[currentStep]}
      </p>
    </div>
  );
}

/* ─── Permission status row ─────────────────────────── */

function PermissionStatusRow({
  granted,
  label,
}: {
  granted: boolean;
  label: string;
}) {
  return (
    <div
      className={cn(
        "flex items-center gap-2.5 rounded-lg border px-3.5 py-2.5 text-sm transition-colors",
        granted
          ? "border-emerald-200 bg-emerald-50/60 dark:border-emerald-800/40 dark:bg-emerald-950/20"
          : "border-border bg-muted/20",
      )}
    >
      {granted ? (
        <CheckCircle2 className="size-4 shrink-0 text-emerald-500" />
      ) : (
        <Circle className="size-4 shrink-0 text-muted-foreground/50" />
      )}
      <span className={granted ? "text-emerald-700 dark:text-emerald-300" : "text-muted-foreground"}>
        {label}
      </span>
    </div>
  );
}

/* ─── Main onboarding component ─────────────────────── */

export default function Onboarding({ onComplete }: OnboardingProps) {
  const [step, setStep] = useState(0);
  const [permissions, setPermissions] = useState<PermissionSnapshot | null>(null);
  const [hasApiKey, setHasApiKey] = useState(false);
  const [chatgptAuthStatus, setChatgptAuthStatus] = useState<ChatGptAuthStatus | null>(null);
  const [selectedAuthMethod, setSelectedAuthMethod] = useState<OnboardingAuthMethod>("oauth");
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
  const [authActionCompleted, setAuthActionCompleted] = useState(false);
  const [practiceStatus, setPracticeStatus] = useState<OnboardingPracticeStatus>("idle");
  const [practiceTranscript, setPracticeTranscript] = useState("");
  const [practiceErrorMessage, setPracticeErrorMessage] = useState("");

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
    return onboardingAuthSuccessMessage({
      chatgptAuthStatus,
      hasApiKey,
      authActionCompleted,
    });
  }, [authActionCompleted, chatgptAuthStatus, hasApiKey]);
  const practiceStatusDescription = useMemo(() => {
    if (practiceStatus === "listening") {
      return "Recording in progress - speak into your mic.";
    }
    if (practiceStatus === "transcribing") {
      return "Transcribing your speech...";
    }
    if (practiceStatus === "error") {
      return practiceErrorMessage || "A recording error occurred while testing.";
    }
    if (practiceTranscript.length > 0) {
      return "Looks good! Review your transcript, then continue.";
    }
    return "Press your shortcut and speak a short phrase to test.";
  }, [practiceErrorMessage, practiceStatus, practiceTranscript]);

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
    if (step !== 3 || !authConfigured || !authActionCompleted) return undefined;

    const timeoutId = window.setTimeout(() => {
      setStep((current) => (current === 3 ? 4 : current));
      setAuthActionCompleted(false);
    }, 250);

    return () => window.clearTimeout(timeoutId);
  }, [authActionCompleted, authConfigured, step]);

  useEffect(() => {
    if (step === 3 || !authActionCompleted) return;
    setAuthActionCompleted(false);
  }, [authActionCompleted, step]);

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

  useEffect(() => {
    if (step !== 5) {
      return undefined;
    }

    setPracticeStatus("idle");
    setPracticeTranscript("");
    setPracticeErrorMessage("");

    let isMounted = true;
    let unlistenFns: UnlistenFn[] = [];

    async function bindPracticeEvents() {
      try {
        const initialStatus = await invoke<OnboardingPracticeStatus>("get_status");
        if (isMounted) {
          setPracticeStatus(initialStatus);
          if (initialStatus !== "error") {
            setPracticeErrorMessage("");
          }
        }
      } catch {
        // Keep onboarding resilient if status sync is unavailable.
      }

      try {
        const listeners = await Promise.all([
          listen<OnboardingPracticeStatus>("voice://status-changed", ({ payload }) => {
            setPracticeStatus(payload);
            if (payload !== "error") {
              setPracticeErrorMessage("");
            }
          }),
          listen<TranscriptReadyEvent>("voice://transcript-ready", ({ payload }) => {
            const transcript = extractTranscriptText(payload);
            if (transcript.length === 0) {
              return;
            }

            setPracticeTranscript(transcript);
            setPracticeStatus("idle");
            setPracticeErrorMessage("");
          }),
          listen<unknown>("voice://transcription-complete", ({ payload }) => {
            const transcript = extractTranscriptText(payload);
            if (transcript.length === 0) {
              return;
            }

            setPracticeTranscript(transcript);
            setPracticeStatus("idle");
            setPracticeErrorMessage("");
          }),
          listen<PipelineErrorEvent>("voice://pipeline-error", ({ payload }) => {
            setPracticeStatus("error");
            setPracticeErrorMessage(
              payload.message?.trim() || "A recording error occurred while testing."
            );
          }),
        ]);

        if (!isMounted) {
          listeners.forEach((dispose) => dispose());
          return;
        }

        unlistenFns = listeners;
      } catch {
        // Keep onboarding usable even if events fail to bind.
      }
    }

    void bindPracticeEvents();

    return () => {
      isMounted = false;
      unlistenFns.forEach((dispose) => dispose());
    };
  }, [step]);

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

  const handleBack = useCallback(() => {
    setErrorMessage("");
    setStep((current) => Math.max(0, current - 1));
  }, []);

  const handleStartOauth = useCallback(async () => {
    setIsStartingOauth(true);
    setErrorMessage("");
    try {
      const status = await invoke<ChatGptAuthStatus>("start_oauth_login");
      setChatgptAuthStatus(status);
      setAuthActionCompleted(true);
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

    setIsSavingApiKey(true);
    setErrorMessage("");
    try {
      await invoke("save_api_key", { provider: OPENAI_PROVIDER, key });
      setHasApiKey(true);
      setApiKeyDraft("");
      setAuthActionCompleted(true);
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

  const handleContinueFromPracticeStep = useCallback(() => {
    setStep(6);
  }, []);

  /* ─── Step renderers ─────────────────────────────── */

  const renderStep = () => {
    /* ── Welcome ── */
    if (step === 0) {
      return (
        <div className="flex flex-col items-center gap-6 py-4 text-center">
          <div className="flex size-16 items-center justify-center rounded-2xl border bg-muted/30">
            <img src="/icon.png" alt="Buzz app icon" className="size-10 rounded-lg" />
          </div>
          <div className="space-y-2">
            <h2 className="text-2xl font-semibold tracking-tight">Welcome to Buzz</h2>
            <p className="mx-auto max-w-sm text-sm leading-relaxed text-muted-foreground">
              Voice-to-text, anywhere on your Mac. Let&apos;s get you set up in about a minute.
            </p>
          </div>
          <Button onClick={() => setStep(1)}>Get Started</Button>
        </div>
      );
    }

    /* ── Microphone ── */
    if (step === 1) {
      return (
        <div className="space-y-5">
          <div className="space-y-1.5">
            <h2 className="text-xl font-semibold tracking-tight">Microphone access</h2>
            <p className="text-sm leading-relaxed text-muted-foreground">
              Buzz needs your microphone to capture speech and turn it into text.
            </p>
          </div>

          <PermissionStatusRow
            granted={Boolean(micGranted)}
            label={micGranted ? "Microphone permission granted" : "Waiting for microphone access"}
          />

          <div className="flex justify-start pt-1">
            <Button
              variant="outline"
              onClick={handleRequestMic}
              disabled={isRequestingMic || Boolean(micGranted)}
            >
              {isRequestingMic ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Requesting…
                </>
              ) : micGranted ? (
                <>
                  <CheckCircle2 className="size-4 text-emerald-500" />
                  Granted
                </>
              ) : (
                <>
                  <Mic className="size-4" />
                  Grant Access
                </>
              )}
            </Button>
          </div>
          <div className="flex items-center justify-between pt-1">
            <Button variant="ghost" onClick={handleBack}>
              <ChevronLeft className="size-4" />
              Back
            </Button>
            <Button onClick={() => setStep(2)} disabled={!micGranted}>
              Continue
            </Button>
          </div>
        </div>
      );
    }

    /* ── Accessibility ── */
    if (step === 2) {
      return (
        <div className="space-y-5">
          <div className="space-y-1.5">
            <h2 className="text-xl font-semibold tracking-tight">Accessibility access</h2>
            <p className="text-sm leading-relaxed text-muted-foreground">
              Buzz uses Accessibility to paste transcribed text at your cursor in any app.
            </p>
          </div>

          <PermissionStatusRow
            granted={Boolean(accessibilityGranted)}
            label={
              accessibilityGranted
                ? "Accessibility permission granted"
                : "Waiting for accessibility access"
            }
          />

          <p className="rounded-lg border bg-muted/20 px-3 py-2 text-xs leading-relaxed text-muted-foreground">
            macOS requires you to manually enable Buzz in{" "}
            <span className="font-medium">System Settings → Privacy & Security → Accessibility</span>{" "}
            after opening the panel.
          </p>

          <div className="flex justify-start pt-1">
            <Button
              variant="outline"
              onClick={handleOpenAccessibilitySettings}
              disabled={isOpeningAccessibilitySettings || Boolean(accessibilityGranted)}
            >
              {isOpeningAccessibilitySettings ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Opening…
                </>
              ) : accessibilityGranted ? (
                <>
                  <CheckCircle2 className="size-4 text-emerald-500" />
                  Granted
                </>
              ) : (
                <>
                  <Shield className="size-4" />
                  Open System Settings
                </>
              )}
            </Button>
          </div>
          <div className="flex items-center justify-between pt-1">
            <Button variant="ghost" onClick={handleBack}>
              <ChevronLeft className="size-4" />
              Back
            </Button>
            <Button onClick={() => setStep(3)} disabled={!accessibilityGranted}>
              Continue
            </Button>
          </div>
        </div>
      );
    }

    /* ── Authentication ── */
    if (step === 3) {
      return (
        <div className="space-y-5">
          <div className="space-y-1.5">
            <h2 className="text-xl font-semibold tracking-tight">Connect to OpenAI</h2>
            <p className="text-sm leading-relaxed text-muted-foreground">
              Choose how Buzz authenticates with OpenAI for transcription.
            </p>
          </div>

          <div className="grid gap-3 sm:grid-cols-2">
            <SelectableCard
              selected={selectedAuthMethod === "oauth"}
              onClick={() => {
                setSelectedAuthMethod("oauth");
                setErrorMessage("");
              }}
            >
              <CardHeader className="pb-2">
                <CardTitle className="text-sm">Sign in with ChatGPT</CardTitle>
                <CardDescription className="text-xs">
                  Opens your browser for a quick OAuth login.
                </CardDescription>
              </CardHeader>
            </SelectableCard>

            <SelectableCard
              selected={selectedAuthMethod === "api_key"}
              onClick={() => {
                setSelectedAuthMethod("api_key");
                setErrorMessage("");
              }}
            >
              <CardHeader className="pb-2">
                <CardTitle className="text-sm">Use API Key</CardTitle>
                <CardDescription className="text-xs">
                  Paste an OpenAI API key if you prefer key-based auth.
                </CardDescription>
              </CardHeader>
            </SelectableCard>
          </div>

          {selectedAuthMethod === "oauth" && (
            <div className="space-y-2.5 rounded-lg border bg-muted/20 p-3.5">
              <Button
                size="sm"
                className="w-full"
                onClick={handleStartOauth}
                disabled={isStartingOauth}
              >
                {isStartingOauth ? (
                  <>
                    <Loader2 className="size-4 animate-spin" />
                    Opening browser…
                  </>
                ) : (
                  "Sign in with ChatGPT"
                )}
              </Button>
            </div>
          )}

          {shouldShowOnboardingApiKeyInput(selectedAuthMethod) && (
            <div className="space-y-2.5 rounded-lg border bg-muted/20 p-3.5">
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
                    Saving…
                  </>
                ) : (
                  "Save API Key"
                )}
              </Button>
            </div>
          )}

          {authSuccessMessage && (
            <div className="flex items-center gap-2.5 rounded-lg border border-emerald-200 bg-emerald-50/60 px-3.5 py-2.5 text-sm text-emerald-700 dark:border-emerald-800/40 dark:bg-emerald-950/20 dark:text-emerald-300">
              <CheckCircle2 className="size-4 shrink-0" />
              <span>{authSuccessMessage}</span>
            </div>
          )}

          <div className="flex items-center justify-between pt-1">
            <Button variant="ghost" onClick={handleBack}>
              <ChevronLeft className="size-4" />
              Back
            </Button>
            <Button onClick={() => setStep(4)} disabled={!authConfigured}>
              Continue
            </Button>
          </div>
        </div>
      );
    }

    /* ── Shortcut + Mode ── */
    if (step === 4) {
      return (
        <div className="space-y-6">
          <div className="space-y-1.5">
            <h2 className="text-xl font-semibold tracking-tight">Recording controls</h2>
            <p className="text-sm leading-relaxed text-muted-foreground">
              Pick your recording mode and shortcut. You can change these anytime in Settings.
            </p>
          </div>

          {/* Mode selection */}
          <div className="space-y-2.5">
            <p className="text-[11px] font-semibold uppercase tracking-widest text-muted-foreground/70">
              Recording Mode
            </p>
            <div className="grid gap-3 sm:grid-cols-2">
              <SelectableCard
                selected={recordingMode === "toggle"}
                onClick={() => setRecordingMode("toggle")}
              >
                <CardHeader className="p-4">
                  <CardTitle className="text-sm">Toggle</CardTitle>
                  <CardDescription className="text-xs">
                    Press once to start, press again to stop.
                  </CardDescription>
                </CardHeader>
              </SelectableCard>

              <SelectableCard
                selected={recordingMode === "hold_to_talk"}
                onClick={() => setRecordingMode("hold_to_talk")}
              >
                <CardHeader className="p-4">
                  <CardTitle className="text-sm">Hold to Talk</CardTitle>
                  <CardDescription className="text-xs">
                    Hold while speaking, release to stop.
                  </CardDescription>
                </CardHeader>
              </SelectableCard>
            </div>
          </div>

          {/* Shortcut selection */}
          <div className="space-y-2.5">
            <p className="text-[11px] font-semibold uppercase tracking-widest text-muted-foreground/70">
              Recording Shortcut
            </p>

            <div className="grid gap-2 sm:grid-cols-2">
              {HOTKEY_PRESETS.map((preset) => {
                const isActive = selectedShortcutPreset === preset.value;
                return (
                  <Button
                    key={preset.value}
                    type="button"
                    variant={isActive ? "default" : "outline"}
                    className="justify-center font-medium"
                    onClick={() => {
                      setIsRecordingShortcut(false);
                      setHotkeyShortcut(preset.value);
                    }}
                  >
                    {preset.label}
                  </Button>
                );
              })}
            </div>

            <Button
              type="button"
              variant={
                isRecordingShortcut || selectedShortcutPreset === CUSTOM_SHORTCUT_PRESET_VALUE
                  ? "default"
                  : "outline"
              }
              className="w-full"
              onClick={() => setIsRecordingShortcut((active) => !active)}
            >
              {isRecordingShortcut ? "Press your key combination…" : "Record Custom Shortcut"}
            </Button>

            {isRecordingShortcut && (
              <p className="text-center text-xs font-medium text-primary">
                Listening for key combination…
              </p>
            )}
          </div>

          {/* Current shortcut preview */}
          <div className="space-y-3 rounded-xl border bg-muted/20 p-5 text-center">
            <p className="text-xs font-semibold uppercase tracking-widest text-muted-foreground/80">
              Your shortcut
            </p>
            <ShortcutKeycaps shortcut={hotkeyShortcut} large />
            <p className="text-xs text-muted-foreground">{stopInstruction(recordingMode)}</p>
          </div>

          <div className="flex items-center justify-between">
            <Button variant="ghost" onClick={handleBack}>
              <ChevronLeft className="size-4" />
              Back
            </Button>
            <Button
              onClick={handleSaveShortcutAndMode}
              disabled={isSavingShortcutSettings}
            >
              {isSavingShortcutSettings ? (
                <>
                  <Loader2 className="size-4 animate-spin" />
                  Saving…
                </>
              ) : (
                "Continue"
              )}
            </Button>
          </div>
        </div>
      );
    }

    /* ── Try It Out ── */
    const modeLabel = recordingMode === "hold_to_talk" ? "Hold to Talk" : "Toggle";

    if (step === 5) {
      const isListening = practiceStatus === "listening";
      const isTranscribing = practiceStatus === "transcribing";
      const hasTranscript = practiceTranscript.length > 0;

      return (
        <div className="space-y-5">
          <div className="space-y-1.5">
            <h2 className="text-xl font-semibold tracking-tight">Try it out</h2>
            <p className="text-sm leading-relaxed text-muted-foreground">
              Give it a spin - trigger your shortcut, say something, and watch the magic.
            </p>
          </div>

          {/* Prominent instruction box */}
          <div
            className={cn(
              "space-y-3 rounded-xl border p-5 text-center transition-all duration-300",
              isListening
                ? "border-primary/60 bg-primary/5"
                : "border-border bg-muted/20",
            )}
          >
            <p className="text-sm font-semibold text-foreground">
              Press {shortcutInstructionLabel} to start recording
            </p>
            <ShortcutKeycaps shortcut={hotkeyShortcut} large />
            <p className="text-xs text-muted-foreground">
              {modeLabel} mode &middot; {stopInstruction(recordingMode)}
            </p>
          </div>

          {/* Status pipeline indicators */}
          <div className="grid gap-2 sm:grid-cols-3">
            <div
              className={cn(
                "flex items-center justify-center gap-2 rounded-lg border px-3 py-2.5 text-xs font-medium transition-all duration-200",
                isListening
                  ? "border-emerald-400/60 bg-emerald-50 text-emerald-700 dark:border-emerald-600/40 dark:bg-emerald-950/25 dark:text-emerald-300"
                  : "border-border bg-muted/20 text-muted-foreground",
              )}
            >
              {isListening && <span className="size-1.5 shrink-0 animate-pulse rounded-full bg-emerald-500" />}
              Recording
            </div>
            <div
              className={cn(
                "flex items-center justify-center gap-2 rounded-lg border px-3 py-2.5 text-xs font-medium transition-all duration-200",
                isTranscribing
                  ? "border-primary/40 bg-primary/10 text-foreground"
                  : "border-border bg-muted/20 text-muted-foreground",
              )}
            >
              {isTranscribing && <Loader2 className="size-3 shrink-0 animate-spin text-primary" />}
              Transcribing
            </div>
            <div
              className={cn(
                "flex items-center justify-center gap-2 rounded-lg border px-3 py-2.5 text-xs font-medium transition-all duration-200",
                hasTranscript && !isListening && !isTranscribing
                  ? "border-emerald-400/60 bg-emerald-50 text-emerald-700 dark:border-emerald-600/40 dark:bg-emerald-950/25 dark:text-emerald-300"
                  : "border-border bg-muted/20 text-muted-foreground",
              )}
            >
              {hasTranscript && !isListening && !isTranscribing && (
                <CheckCircle2 className="size-3 shrink-0 text-emerald-500" />
              )}
              Result
            </div>
          </div>

          {/* Transcript area */}
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <p className="text-[11px] font-semibold uppercase tracking-widest text-muted-foreground/70">
                Transcript
              </p>
              <span
                className={cn(
                  "rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wider",
                  practiceStatus === "listening" && "border-emerald-300 bg-emerald-50 text-emerald-600 dark:border-emerald-700 dark:bg-emerald-950/30 dark:text-emerald-400",
                  practiceStatus === "transcribing" && "border-primary/40 bg-primary/10 text-foreground",
                  practiceStatus === "error" && "border-red-300 bg-red-50 text-red-600 dark:border-red-700 dark:bg-red-950/30 dark:text-red-400",
                  practiceStatus === "idle" && "border-border bg-muted/30 text-muted-foreground",
                )}
              >
                {practiceStatusLabel(practiceStatus)}
              </span>
            </div>
            <textarea
              value={practiceTranscript}
              readOnly
              placeholder="Your transcript will appear here…"
              className="min-h-[100px] w-full resize-none rounded-lg border bg-muted/15 px-3.5 py-2.5 text-sm leading-relaxed placeholder:text-muted-foreground/40 outline-none transition-colors focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-ring/30"
            />
            <p className="text-xs leading-relaxed text-muted-foreground">{practiceStatusDescription}</p>
          </div>

          {practiceErrorMessage && (
            <Alert variant="destructive" className="py-2">
              <AlertDescription className="text-xs">{practiceErrorMessage}</AlertDescription>
            </Alert>
          )}

          <div className="flex items-center justify-between pt-1">
            <Button variant="ghost" onClick={handleBack}>
              <ChevronLeft className="size-4" />
              Back
            </Button>
            <Button onClick={handleContinueFromPracticeStep}>
              Continue
            </Button>
          </div>
        </div>
      );
    }

    /* ── All Done ── */
    return (
      <div className="flex flex-col items-center gap-6 py-2 text-center">
        <div className="flex size-16 items-center justify-center rounded-2xl border bg-muted/30">
          <span className="text-3xl" role="img" aria-label="party">
            🎉
          </span>
        </div>

        <div className="space-y-2">
          <h2 className="text-2xl font-semibold tracking-tight">You&apos;re all set!</h2>
          <p className="mx-auto max-w-xs text-sm leading-relaxed text-muted-foreground">
            Buzz is ready to go. Here&apos;s your recording shortcut for quick reference.
          </p>
        </div>

        <div className="w-full max-w-sm space-y-4 rounded-xl border bg-muted/20 p-5">
          <div className="space-y-2.5">
            <p className="text-[11px] font-semibold uppercase tracking-widest text-muted-foreground/80">
              Your shortcut
            </p>
            <ShortcutKeycaps shortcut={hotkeyShortcut} large />
          </div>

          <div className="flex items-center justify-center">
            <span className="rounded-full border bg-background/70 px-3 py-1 text-xs font-medium text-foreground/90">
              {modeLabel} &middot; {stopInstruction(recordingMode)}
            </span>
          </div>
        </div>

        <div className="flex w-full max-w-sm items-center justify-between">
          <Button variant="ghost" onClick={handleBack}>
            <ChevronLeft className="size-4" />
            Back
          </Button>
          <Button onClick={handleCompleteOnboarding} disabled={isCompleting} size="lg">
            {isCompleting ? (
              <>
                <Loader2 className="size-4 animate-spin" />
                Finishing up…
              </>
            ) : (
              <>
                <Sparkles className="size-4" />
                Start Using Buzz
              </>
            )}
          </Button>
        </div>
      </div>
    );
  };

  /* ─── Shell layout ───────────────────────────────── */

  return (
    <main className="flex min-h-screen items-center justify-center bg-background px-6 py-8">
      <Card className="w-full max-w-2xl border-border/60 bg-card">
        <CardHeader className="space-y-4 pb-0">
          <div className="flex items-center justify-center text-[11px] font-medium text-muted-foreground/60">
            First-run setup
          </div>

          <ProgressDots currentStep={step} />
        </CardHeader>

        <CardContent className="px-8 py-6">
          {isLoadingInitialState ? (
            <div className="flex flex-col items-center justify-center gap-3 py-16 text-sm text-muted-foreground">
              <Loader2 className="size-5 animate-spin text-primary" />
              <span>Preparing setup…</span>
            </div>
          ) : (
            <div key={step} className="animate-step-enter">
              {renderStep()}
            </div>
          )}

          {errorMessage && (
            <Alert variant="destructive" className="mt-5 py-2.5">
              <AlertDescription className="text-xs">{errorMessage}</AlertDescription>
            </Alert>
          )}
        </CardContent>
      </Card>
    </main>
  );
}
