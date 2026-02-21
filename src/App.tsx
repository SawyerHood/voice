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
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import { useDarkMode } from "@/hooks/use-dark-mode";
import HistoryPanel from "./HistoryPanel";
import Settings from "./Settings";

type AppStatus = "idle" | "listening" | "transcribing" | "error";
type AppView = "dashboard" | "history" | "settings";
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

/* ─── Sidebar Nav Item ─────────────────────────────── */
type NavItemProps = {
  icon: React.ReactNode;
  label: string;
  active: boolean;
  onClick: () => void;
  badge?: React.ReactNode;
};

function NavItem({ icon, label, active, onClick, badge }: NavItemProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-[13px] font-medium transition-colors",
        active
          ? "bg-sidebar-accent text-sidebar-accent-foreground"
          : "text-sidebar-foreground/60 hover:bg-sidebar-accent/50 hover:text-sidebar-foreground"
      )}
    >
      {icon}
      <span className="flex-1 text-left truncate">{label}</span>
      {badge}
    </button>
  );
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

/* ─── Dashboard View ────────────────────────────────── */
type DashboardViewProps = {
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

function DashboardView({
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
}: DashboardViewProps) {
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

/* ─── View Title Map ────────────────────────────────── */
const VIEW_TITLES: Record<AppView, string> = {
  dashboard: "Dashboard",
  history: "History",
  settings: "Settings",
};

/* ─── Main App ──────────────────────────────────────── */
function App() {
  useDarkMode();

  const [status, setStatus] = useState<AppStatus>("idle");
  const [activeView, setActiveView] = useState<AppView>("dashboard");
  const [errorMessage, setErrorMessage] = useState("");
  const [audioLevel, setAudioLevel] = useState(0);
  const [lastTranscript, setLastTranscript] = useState("");
  const [historyRefreshSignal, setHistoryRefreshSignal] = useState(0);
  const [backendSynced, setBackendSynced] = useState<boolean>(true);
  const activeViewRef = useRef<AppView>(activeView);

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
    activeViewRef.current = activeView;
  }, [activeView]);

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
            if (activeViewRef.current === "history") {
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

  /* Status dot color for the sidebar indicator */
  const statusDotColor = useMemo(() => {
    switch (status) {
      case "listening":
        return "bg-blue-500";
      case "transcribing":
        return "bg-amber-500";
      case "error":
        return "bg-destructive";
      default:
        return "bg-emerald-500";
    }
  }, [status]);

  return (
    <TooltipProvider delayDuration={400}>
      <main className="flex h-screen overflow-hidden">
        {/* ─── Left Sidebar ─── */}
        <aside className="flex w-[180px] shrink-0 flex-col border-r border-sidebar-border bg-sidebar">
          {/* App identity */}
          <div className="flex items-center gap-2 px-3 pt-4 pb-3" data-tauri-drag-region="">
            <div className={cn(
              "flex size-7 items-center justify-center rounded-lg bg-sidebar-primary",
            )}>
              <Mic className="size-3.5 text-sidebar-primary-foreground" />
            </div>
            <div className="min-w-0 flex-1">
              <p className="text-sm font-semibold text-sidebar-foreground tracking-tight">Voice</p>
            </div>
            <Tooltip>
              <TooltipTrigger asChild>
                <div className={cn("size-2 shrink-0 rounded-full", statusDotColor)} />
              </TooltipTrigger>
              <TooltipContent side="right" className="text-xs">
                {STATUS_LABEL[status]}
              </TooltipContent>
            </Tooltip>
          </div>

          <Separator className="bg-sidebar-border" />

          {/* Primary nav */}
          <nav className="flex flex-1 flex-col gap-0.5 px-2 py-2">
            <NavItem
              icon={<Mic className="size-4 shrink-0" />}
              label="Dashboard"
              active={activeView === "dashboard"}
              onClick={() => setActiveView("dashboard")}
            />
            <NavItem
              icon={<History className="size-4 shrink-0" />}
              label="History"
              active={activeView === "history"}
              onClick={() => setActiveView("history")}
            />

            {/* Spacer to push settings to bottom */}
            <div className="flex-1" />

            <Separator className="my-1 bg-sidebar-border" />

            <NavItem
              icon={<SettingsIcon className="size-4 shrink-0" />}
              label="Settings"
              active={activeView === "settings"}
              onClick={() => setActiveView("settings")}
            />
          </nav>

          {/* Backend sync warning in sidebar footer */}
          {!backendSynced && (
            <div className="px-3 pb-2">
              <p className="text-[10px] text-sidebar-foreground/50 text-center">
                Backend offline
              </p>
            </div>
          )}
        </aside>

        {/* ─── Right Content ─── */}
        <div className="flex min-w-0 flex-1 flex-col">
          {/* Content header */}
          <header className="shrink-0 border-b px-4 py-3" data-tauri-drag-region="">
            <h1 className="text-sm font-semibold tracking-tight">
              {VIEW_TITLES[activeView]}
            </h1>
          </header>

          {/* Content body */}
          <ScrollArea className="flex-1">
            <div className="p-4">
              {activeView === "dashboard" && (
                <DashboardView
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
              )}
              {activeView === "history" && (
                <HistoryPanel refreshSignal={historyRefreshSignal} />
              )}
              {activeView === "settings" && (
                <Settings />
              )}
            </div>
          </ScrollArea>
        </div>
      </main>
    </TooltipProvider>
  );
}

export default App;
