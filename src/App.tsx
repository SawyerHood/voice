import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  Mic,
  History,
  Settings as SettingsIcon,
  ShieldCheck,
  ShieldAlert,
  ShieldQuestion,
  RefreshCw,
  X,
} from "lucide-react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { cn } from "@/lib/utils";
import { useDarkMode } from "@/hooks/use-dark-mode";
import HistoryPanel from "./HistoryPanel";
import Settings from "./Settings";

type AppStatus = "idle" | "listening" | "transcribing" | "error";
type AppTab = "status" | "history" | "settings";
type PermissionState = "not_determined" | "granted" | "denied";
type PermissionType = "microphone" | "accessibility";
type TranscriptReadyEvent = { text: string };
type PipelineErrorEvent = { stage: string; message: string };
type PermissionSnapshot = {
  microphone: PermissionState;
  accessibility: PermissionState;
  allGranted: boolean;
};

const STATUS_LABEL: Record<AppStatus, string> = {
  idle: "Idle",
  listening: "Listening",
  transcribing: "Transcribing",
  error: "Error",
};

const STATUS_DESC: Record<AppStatus, string> = {
  idle: "Waiting for the global hotkey.",
  listening: "Capturing microphone input.",
  transcribing: "Converting audio to text.",
  error: "A recoverable issue occurred.",
};

const PERMISSION_LABEL: Record<PermissionType, string> = {
  microphone: "Microphone",
  accessibility: "Accessibility",
};

const PERMISSION_HELP: Record<PermissionType, string> = {
  microphone: "Required to capture speech audio.",
  accessibility: "Required to type/paste into other apps.",
};

const PERMISSION_CARD_DISMISSED_KEY = "voice.permissionsOnboardingDismissed.v1";

function toErrorMessage(error: unknown, fallbackMessage: string): string {
  if (typeof error === "string" && error.trim()) return error;
  if (error instanceof Error && error.message.trim()) return error.message;
  return fallbackMessage;
}

function readPermissionCardDismissed(): boolean {
  try {
    return window.localStorage.getItem(PERMISSION_CARD_DISMISSED_KEY) === "1";
  } catch {
    return false;
  }
}

function setPermissionCardDismissed(dismissed: boolean) {
  try {
    if (dismissed) {
      window.localStorage.setItem(PERMISSION_CARD_DISMISSED_KEY, "1");
      return;
    }
    window.localStorage.removeItem(PERMISSION_CARD_DISMISSED_KEY);
  } catch {
    // Ignore storage write errors
  }
}

/* ─── Permission status icon ───────────────────────── */
function PermissionIcon({ state }: { state: PermissionState }) {
  if (state === "granted")
    return <ShieldCheck className="size-4 text-emerald-500" />;
  if (state === "denied")
    return <ShieldAlert className="size-4 text-destructive" />;
  return <ShieldQuestion className="size-4 text-amber-500" />;
}

/* ─── Permission onboarding card ────────────────────── */
type PermissionCardProps = {
  errorMessage: string;
  isRefreshing: boolean;
  onDismiss: () => void;
  onRefresh: () => void;
  onRequestPermission: (permission: PermissionType) => void;
  permissions: PermissionSnapshot | null;
  requestingPermission: PermissionType | null;
  showDismiss: boolean;
};

function PermissionOnboardingCard({
  errorMessage,
  isRefreshing,
  onDismiss,
  onRefresh,
  onRequestPermission,
  permissions,
  requestingPermission,
  showDismiss,
}: PermissionCardProps) {
  const permissionOrder: PermissionType[] = ["microphone", "accessibility"];

  return (
    <Card className="border-amber-500/30 bg-amber-50/50 dark:bg-amber-950/20">
      <CardHeader className="pb-3">
        <div className="flex items-start justify-between">
          <div>
            <CardTitle className="text-sm">Permissions Required</CardTitle>
            <CardDescription className="text-xs mt-1">
              {permissions?.allGranted
                ? "All required permissions are granted."
                : "Voice needs these macOS permissions to record and insert text."}
            </CardDescription>
          </div>
          {showDismiss && (
            <Button variant="ghost" size="icon-xs" onClick={onDismiss}>
              <X className="size-3.5" />
            </Button>
          )}
        </div>
      </CardHeader>
      <CardContent className="space-y-2">
        {errorMessage && (
          <Alert variant="destructive" className="py-2">
            <AlertDescription className="text-xs">{errorMessage}</AlertDescription>
          </Alert>
        )}

        {permissionOrder.map((permType) => {
          const state = permissions?.[permType] ?? "not_determined";
          const isGranted = state === "granted";
          const isRequesting = requestingPermission === permType;

          return (
            <div
              key={permType}
              className="flex items-center justify-between rounded-lg border bg-background/60 p-2.5"
            >
              <div className="flex items-center gap-2.5">
                <PermissionIcon state={state} />
                <div>
                  <p className="text-xs font-medium">{PERMISSION_LABEL[permType]}</p>
                  <p className="text-[11px] text-muted-foreground">{PERMISSION_HELP[permType]}</p>
                </div>
              </div>
              <div className="flex items-center gap-2">
                <Badge
                  variant={isGranted ? "default" : state === "denied" ? "destructive" : "secondary"}
                  className="text-[10px] px-1.5 py-0"
                >
                  {isGranted ? "Granted" : state === "denied" ? "Denied" : "Needs Access"}
                </Badge>
                {!isGranted && (
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={isRequesting}
                    onClick={() => onRequestPermission(permType)}
                  >
                    {isRequesting ? "Requesting..." : "Grant"}
                  </Button>
                )}
              </div>
            </div>
          );
        })}

        <div className="flex justify-end pt-1">
          <Button
            variant="ghost"
            size="xs"
            onClick={onRefresh}
            disabled={isRefreshing || requestingPermission !== null}
          >
            <RefreshCw className={cn("size-3", isRefreshing && "animate-spin")} />
            {isRefreshing ? "Refreshing..." : "Refresh Status"}
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}

/* ─── Status View ───────────────────────────────────── */
type StatusViewProps = {
  audioLevel: number;
  isRefreshingPermissions: boolean;
  lastTranscript: string;
  onDismissPermissions: () => void;
  onRefreshPermissions: () => void;
  onRequestPermission: (permission: PermissionType) => void;
  permissionErrorMessage: string;
  permissions: PermissionSnapshot | null;
  requestingPermission: PermissionType | null;
  showPermissionsCard: boolean;
  status: AppStatus;
  statusDescription: string;
};

function StatusView({
  audioLevel,
  isRefreshingPermissions,
  lastTranscript,
  onDismissPermissions,
  onRefreshPermissions,
  onRequestPermission,
  permissionErrorMessage,
  permissions,
  requestingPermission,
  showPermissionsCard,
  status,
  statusDescription,
}: StatusViewProps) {
  const statusColors: Record<AppStatus, string> = {
    idle: "bg-muted-foreground",
    listening: "bg-blue-500 animate-pulse-dot",
    transcribing: "bg-amber-500 animate-pulse-dot-fast",
    error: "bg-destructive",
  };

  const statusRingColors: Record<AppStatus, string> = {
    idle: "",
    listening: "ring-2 ring-blue-500/20",
    transcribing: "ring-2 ring-amber-500/20",
    error: "ring-2 ring-destructive/20",
  };

  return (
    <div className="space-y-3">
      {showPermissionsCard && (
        <PermissionOnboardingCard
          errorMessage={permissionErrorMessage}
          isRefreshing={isRefreshingPermissions}
          onDismiss={onDismissPermissions}
          onRefresh={onRefreshPermissions}
          onRequestPermission={onRequestPermission}
          permissions={permissions}
          requestingPermission={requestingPermission}
          showDismiss={Boolean(permissions?.allGranted)}
        />
      )}

      {/* Status indicator */}
      <Card className={cn("transition-all duration-200", statusRingColors[status])}>
        <CardContent className="flex items-center gap-3 py-4">
          <div className={cn("size-2.5 shrink-0 rounded-full", statusColors[status])} />
          <div className="min-w-0 flex-1">
            <p className="text-sm font-semibold">{STATUS_LABEL[status]}</p>
            <p className="text-xs text-muted-foreground">{statusDescription}</p>
            {status === "transcribing" && (
              <div className="mt-2 h-1 w-full overflow-hidden rounded-full bg-muted">
                <div className="h-full w-2/5 rounded-full bg-amber-500 animate-shimmer" />
              </div>
            )}
          </div>
        </CardContent>
      </Card>

      {/* Audio Level */}
      <Card>
        <CardContent className="py-4 space-y-2.5">
          <p className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            Audio Level
          </p>
          <Progress
            value={status === "listening" ? Math.round(audioLevel * 100) : 0}
            className="h-2"
          />
          <p className="text-xs text-muted-foreground tabular-nums">
            {status === "listening"
              ? `${Math.round(audioLevel * 100)}% input`
              : "Waiting for recording"}
          </p>
        </CardContent>
      </Card>

      {/* Last Transcript */}
      <Card>
        <CardContent className="py-4 space-y-2">
          <p className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
            Last Transcript
          </p>
          <p
            className={cn(
              "text-sm leading-relaxed",
              lastTranscript ? "text-foreground" : "text-muted-foreground italic"
            )}
          >
            {lastTranscript || "No transcript captured yet."}
          </p>
        </CardContent>
      </Card>
    </div>
  );
}

/* ─── Main App ──────────────────────────────────────── */
function App() {
  useDarkMode();

  const [status, setStatus] = useState<AppStatus>("idle");
  const [activeTab, setActiveTab] = useState<AppTab>("status");
  const [errorMessage, setErrorMessage] = useState("");
  const [audioLevel, setAudioLevel] = useState(0);
  const [lastTranscript, setLastTranscript] = useState("");
  const [historyRefreshSignal, setHistoryRefreshSignal] = useState(0);
  const [backendSynced, setBackendSynced] = useState<boolean>(true);
  const activeTabRef = useRef<AppTab>(activeTab);

  const [permissions, setPermissions] = useState<PermissionSnapshot | null>(null);
  const [permissionErrorMessage, setPermissionErrorMessage] = useState("");
  const [requestingPermission, setRequestingPermission] = useState<PermissionType | null>(null);
  const [isRefreshingPermissions, setIsRefreshingPermissions] = useState(false);
  const [permissionCardDismissed, setPermissionCardDismissedState] = useState<boolean>(
    () => readPermissionCardDismissed()
  );
  const statusRef = useRef<AppStatus>("idle");

  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  const refreshPermissions = useCallback(async () => {
    setIsRefreshingPermissions(true);
    try {
      const snapshot = await invoke<PermissionSnapshot>("check_permissions");
      setPermissions(snapshot);
      setPermissionErrorMessage("");
    } catch (error) {
      setPermissionErrorMessage(
        toErrorMessage(error, "Unable to load permission status from the backend.")
      );
    } finally {
      setIsRefreshingPermissions(false);
    }
  }, []);

  const requestPermission = useCallback(async (permission: PermissionType) => {
    setRequestingPermission(permission);
    setPermissionErrorMessage("");
    try {
      const snapshot = await invoke<PermissionSnapshot>("request_permission", {
        type: permission,
      });
      setPermissions(snapshot);
    } catch (error) {
      setPermissionErrorMessage(
        toErrorMessage(error, "Unable to request macOS permission from the backend.")
      );
    } finally {
      setRequestingPermission(null);
    }
  }, []);

  const dismissPermissionCard = useCallback(() => {
    setPermissionCardDismissedState(true);
    setPermissionCardDismissed(true);
  }, []);

  useEffect(() => {
    activeTabRef.current = activeTab;
  }, [activeTab]);

  useEffect(() => {
    let isMounted = true;
    let unlistenFns: UnlistenFn[] = [];

    async function bindBackend() {
      try {
        const [initialStatus, initialAudioLevel, initialPermissions] = await Promise.all([
          invoke<AppStatus>("get_status"),
          invoke<number>("get_audio_level"),
          invoke<PermissionSnapshot>("check_permissions"),
        ]);

        if (!isMounted) return;

        setStatus(initialStatus);
        statusRef.current = initialStatus;
        setAudioLevel(Number.isFinite(initialAudioLevel) ? initialAudioLevel : 0);
        setPermissions(initialPermissions);
        setPermissionErrorMessage("");
      } catch {
        if (isMounted) {
          setBackendSynced(false);
          setPermissionErrorMessage("Unable to load permission status from the backend.");
        }
      }

      try {
        const listeners = await Promise.all([
          listen<AppStatus>("voice://status-changed", ({ payload }) => {
            statusRef.current = payload;
            setStatus(payload);
            if (payload !== "error") setErrorMessage("");
          }),
          listen<number>("audio-level", ({ payload }) => {
            const normalized = Math.max(0, Math.min(1, Number(payload) || 0));
            if (statusRef.current !== "listening" && normalized > 0) return;
            const quantized = Math.round(normalized * 100) / 100;
            setAudioLevel((previous) =>
              Math.abs(previous - quantized) < 0.01 ? previous : quantized
            );
          }),
          listen<TranscriptReadyEvent>("voice://transcript-ready", ({ payload }) => {
            setLastTranscript(payload.text ?? "");
            if (activeTabRef.current === "history") {
              setHistoryRefreshSignal((current) => current + 1);
            }
          }),
          listen<PipelineErrorEvent>("voice://pipeline-error", ({ payload }) => {
            setErrorMessage(payload.message || "An unexpected pipeline error occurred.");
            statusRef.current = "error";
            setStatus("error");
          }),
        ]);

        if (!isMounted) {
          listeners.forEach((dispose) => dispose());
          return;
        }

        unlistenFns = listeners;
        setBackendSynced(true);
      } catch {
        if (isMounted) setBackendSynced(false);
      }
    }

    void bindBackend();

    return () => {
      isMounted = false;
      unlistenFns.forEach((dispose) => dispose());
    };
  }, []);

  useEffect(() => {
    function handleWindowFocus() {
      void refreshPermissions();
    }
    window.addEventListener("focus", handleWindowFocus);
    return () => window.removeEventListener("focus", handleWindowFocus);
  }, [refreshPermissions]);

  const hasMissingPermissions = useMemo(
    () => !permissions || !permissions.allGranted,
    [permissions]
  );

  const showPermissionsCard = useMemo(() => {
    if (!backendSynced) return false;
    return hasMissingPermissions || !permissionCardDismissed;
  }, [backendSynced, hasMissingPermissions, permissionCardDismissed]);

  const statusDescription = useMemo(() => {
    if (status === "error") return errorMessage || STATUS_DESC.error;
    return STATUS_DESC[status] ?? "Unknown state.";
  }, [errorMessage, status]);

  return (
    <main className="flex h-screen flex-col overflow-hidden p-3">
      {/* Header */}
      <header className="mb-2 shrink-0">
        <p className="text-[10px] font-medium uppercase tracking-widest text-muted-foreground">
          Voice Utility
        </p>
        <h1 className="text-lg font-bold tracking-tight">
          {activeTab === "status" ? "Status" : activeTab === "history" ? "History" : "Settings"}
        </h1>
      </header>

      {/* Tabs */}
      <Tabs
        value={activeTab}
        onValueChange={(v) => setActiveTab(v as AppTab)}
        className="flex min-h-0 flex-1 flex-col"
      >
        <TabsList className="mb-3 grid w-full shrink-0 grid-cols-3">
          <TabsTrigger value="status" className="gap-1.5 text-xs">
            <Mic className="size-3.5" />
            Status
          </TabsTrigger>
          <TabsTrigger value="history" className="gap-1.5 text-xs">
            <History className="size-3.5" />
            History
          </TabsTrigger>
          <TabsTrigger value="settings" className="gap-1.5 text-xs">
            <SettingsIcon className="size-3.5" />
            Settings
          </TabsTrigger>
        </TabsList>

        <TabsContent value="status" className="mt-0 min-h-0 flex-1 overflow-y-auto">
          <StatusView
            audioLevel={audioLevel}
            isRefreshingPermissions={isRefreshingPermissions}
            lastTranscript={lastTranscript}
            onDismissPermissions={dismissPermissionCard}
            onRefreshPermissions={() => void refreshPermissions()}
            onRequestPermission={requestPermission}
            permissionErrorMessage={permissionErrorMessage}
            permissions={permissions}
            requestingPermission={requestingPermission}
            showPermissionsCard={showPermissionsCard}
            status={status}
            statusDescription={statusDescription}
          />
        </TabsContent>

        <TabsContent value="history" className="mt-0 min-h-0 flex-1 overflow-y-auto">
          <HistoryPanel refreshSignal={historyRefreshSignal} />
        </TabsContent>

        <TabsContent value="settings" className="mt-0 min-h-0 flex-1 overflow-y-auto">
          <Settings />
        </TabsContent>
      </Tabs>

      {/* Backend sync warning */}
      {!backendSynced && (
        <p className="mt-1 shrink-0 text-center text-[11px] text-muted-foreground">
          Backend: frontend-only fallback
        </p>
      )}
    </main>
  );
}

export default App;
