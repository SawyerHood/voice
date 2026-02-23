import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { check, type Update } from "@tauri-apps/plugin-updater";
import {
  Mic,
  History,
  Settings as SettingsIcon,
  ShieldCheck,
  ShieldAlert,
  ShieldQuestion,
  RefreshCw,
  RotateCcw,
  X,
} from "lucide-react";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Progress } from "@/components/ui/progress";
import { Alert, AlertDescription } from "@/components/ui/alert";
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
import Onboarding from "./Onboarding";

type AppStatus = "idle" | "listening" | "transcribing" | "error";
type AppView = "dashboard" | "history" | "settings";
type OnboardingState = "loading" | "required" | "completed";
type PermissionState = "not_determined" | "granted" | "denied";
type PermissionType = "microphone" | "accessibility";
type TranscriptReadyEvent = { text: string };
type PipelineErrorEvent = { stage: string; message: string };
type PermissionSnapshot = {
  microphone: PermissionState;
  accessibility: PermissionState;
  allGranted: boolean;
};
type DailyUsageStats = {
  transcriptions: number;
  words: number;
  recordingSeconds: number;
};
type DailyWordCount = {
  date: string;
  words: number;
};
type UsageStatsReport = {
  totalTranscriptions: number;
  totalWords: number;
  totalRecordingSeconds: number;
  wordsPerMinute: number;
  averageTranscriptionLength: number;
  streakDays: number;
  today: DailyUsageStats;
  dailyWordHistory: DailyWordCount[];
  lastUpdated: string;
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
const INTEGER_FORMATTER = new Intl.NumberFormat();
const DAY_LABEL_FORMATTER = new Intl.DateTimeFormat(undefined, { weekday: "short" });

function toErrorMessage(error: unknown, fallbackMessage: string): string {
  if (typeof error === "string" && error.trim()) return error;
  if (error instanceof Error && error.message.trim()) return error.message;
  return fallbackMessage;
}

function toIsoDateKey(date: Date): string {
  const year = date.getFullYear();
  const month = `${date.getMonth() + 1}`.padStart(2, "0");
  const day = `${date.getDate()}`.padStart(2, "0");
  return `${year}-${month}-${day}`;
}

function buildFallbackDailyWordHistory(days: number): DailyWordCount[] {
  const today = new Date();
  today.setHours(0, 0, 0, 0);

  return Array.from({ length: days }, (_, index) => {
    const date = new Date(today);
    date.setDate(today.getDate() - (days - 1 - index));
    return {
      date: toIsoDateKey(date),
      words: 0,
    };
  });
}

function formatInteger(value: number): string {
  if (!Number.isFinite(value)) return "0";
  return INTEGER_FORMATTER.format(Math.max(0, Math.round(value)));
}

function formatMetric(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0.0";
  return value.toFixed(1);
}

function dayLabelFromDateKey(dateKey: string): string {
  const date = new Date(`${dateKey}T00:00:00`);
  if (Number.isNaN(date.getTime())) return "--";
  return DAY_LABEL_FORMATTER.format(date);
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
                : "Buzz needs these macOS permissions to record and insert text."}
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
  isRefreshingUsageStats: boolean;
  isResettingUsageStats: boolean;
  lastTranscript: string;
  onDismissPermissions: () => void;
  onRefreshPermissions: () => void;
  onRefreshUsageStats: () => void;
  onResetUsageStats: () => void;
  onRequestPermission: (permission: PermissionType) => void;
  permissionErrorMessage: string;
  permissions: PermissionSnapshot | null;
  requestingPermission: PermissionType | null;
  showPermissionsCard: boolean;
  status: AppStatus;
  statusDescription: string;
  usageStats: UsageStatsReport | null;
  usageStatsErrorMessage: string;
};

function DashboardView({
  audioLevel,
  isRefreshingPermissions,
  isRefreshingUsageStats,
  isResettingUsageStats,
  lastTranscript,
  onDismissPermissions,
  onRefreshPermissions,
  onRefreshUsageStats,
  onResetUsageStats,
  onRequestPermission,
  permissionErrorMessage,
  permissions,
  requestingPermission,
  showPermissionsCard,
  status,
  statusDescription,
  usageStats,
  usageStatsErrorMessage,
}: DashboardViewProps) {
  const statusColors: Record<AppStatus, string> = {
    idle: "bg-muted-foreground",
    listening: "bg-emerald-500 animate-pulse-dot",
    transcribing: "bg-amber-500 animate-pulse-dot-fast",
    error: "bg-destructive",
  };

  const statusRingColors: Record<AppStatus, string> = {
    idle: "",
    listening: "ring-2 ring-emerald-500/20",
    transcribing: "ring-2 ring-amber-500/20",
    error: "ring-2 ring-destructive/20",
  };

  const dailyWordHistory = usageStats?.dailyWordHistory?.length
    ? usageStats.dailyWordHistory
    : buildFallbackDailyWordHistory(30);
  const chartPoints = dailyWordHistory.slice(-14).map((point) => ({
    ...point,
    dayLabel: dayLabelFromDateKey(point.date),
  }));
  const maxChartWords = chartPoints.reduce(
    (highest, point) => Math.max(highest, point.words),
    0
  );

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

      {/* Usage Stats */}
      <Card>
        <CardHeader className="pb-3">
          <div className="flex items-start justify-between gap-2">
            <div>
              <CardTitle className="text-sm uppercase tracking-wider">Usage Stats</CardTitle>
              <CardDescription className="text-xs mt-1">
                Totals from successful transcriptions and inserts.
              </CardDescription>
            </div>
            <div className="flex items-center gap-1.5">
              <Button
                variant="ghost"
                size="xs"
                onClick={onRefreshUsageStats}
                disabled={isRefreshingUsageStats || isResettingUsageStats}
              >
                <RefreshCw className={cn("size-3", isRefreshingUsageStats && "animate-spin")} />
                {isRefreshingUsageStats ? "Refreshing..." : "Refresh"}
              </Button>
              <Button
                variant="ghost"
                size="xs"
                className="text-destructive hover:text-destructive"
                onClick={onResetUsageStats}
                disabled={isResettingUsageStats || isRefreshingUsageStats}
              >
                <RotateCcw className="size-3" />
                {isResettingUsageStats ? "Resetting..." : "Reset Stats"}
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          {usageStatsErrorMessage && (
            <Alert variant="destructive" className="py-2">
              <AlertDescription className="text-xs">{usageStatsErrorMessage}</AlertDescription>
            </Alert>
          )}

          <div className="grid grid-cols-2 gap-2">
            <div className="rounded-lg border bg-background/60 p-2.5">
              <p className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                Total Transcriptions
              </p>
              <p className="mt-1 text-lg font-semibold tabular-nums">
                {formatInteger(usageStats?.totalTranscriptions ?? 0)}
              </p>
            </div>
            <div className="rounded-lg border bg-background/60 p-2.5">
              <p className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                Total Words
              </p>
              <p className="mt-1 text-lg font-semibold tabular-nums">
                {formatInteger(usageStats?.totalWords ?? 0)}
              </p>
            </div>
            <div className="rounded-lg border bg-background/60 p-2.5">
              <p className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                Words / Minute
              </p>
              <p className="mt-1 text-base font-semibold tabular-nums">
                {formatMetric(usageStats?.wordsPerMinute ?? 0)}
              </p>
            </div>
            <div className="rounded-lg border bg-background/60 p-2.5">
              <p className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                Average Length
              </p>
              <p className="mt-1 text-base font-semibold tabular-nums">
                {formatMetric(usageStats?.averageTranscriptionLength ?? 0)} words
              </p>
            </div>
            <div className="rounded-lg border bg-background/60 p-2.5">
              <p className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                Today
              </p>
              <p className="mt-1 text-sm font-semibold tabular-nums">
                {formatInteger(usageStats?.today.transcriptions ?? 0)} transcriptions
              </p>
              <p className="text-xs text-muted-foreground tabular-nums">
                {formatInteger(usageStats?.today.words ?? 0)} words
              </p>
            </div>
            <div className="rounded-lg border bg-background/60 p-2.5">
              <p className="text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
                Streak
              </p>
              <p className="mt-1 text-sm font-semibold tabular-nums">
                {formatInteger(usageStats?.streakDays ?? 0)} day streak {"🔥"}
              </p>
            </div>
          </div>

          <div className="space-y-2">
            <p className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
              Last 14 Days (Words)
            </p>
            <div className="rounded-lg border bg-background/60 px-2.5 py-3">
              <div className="flex h-24 items-end gap-1.5">
                {chartPoints.map((point) => {
                  const ratio = maxChartWords > 0 ? point.words / maxChartWords : 0;
                  const minHeight = point.words > 0 ? 10 : 4;
                  const heightPercent = Math.max(Math.round(ratio * 100), minHeight);

                  return (
                    <div key={point.date} className="flex min-w-0 flex-1 flex-col items-center gap-1">
                      <div className="flex h-16 w-full items-end justify-center">
                        <div
                          className={cn(
                            "w-full rounded-sm transition-all",
                            point.words > 0 ? "bg-primary/80" : "bg-muted-foreground/20"
                          )}
                          style={{ height: `${heightPercent}%` }}
                          title={`${formatInteger(point.words)} words on ${point.date}`}
                        />
                      </div>
                      <span className="text-[10px] text-muted-foreground">{point.dayLabel}</span>
                    </div>
                  );
                })}
              </div>
            </div>
          </div>
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

  const [onboardingState, setOnboardingState] = useState<OnboardingState>("loading");
  const [status, setStatus] = useState<AppStatus>("idle");
  const [activeView, setActiveView] = useState<AppView>("dashboard");
  const [errorMessage, setErrorMessage] = useState("");
  const [audioLevel, setAudioLevel] = useState(0);
  const [lastTranscript, setLastTranscript] = useState("");
  const [historyRefreshSignal, setHistoryRefreshSignal] = useState(0);
  const [backendSynced, setBackendSynced] = useState<boolean>(true);
  const [availableUpdate, setAvailableUpdate] = useState<Update | null>(null);
  const [isInstallingUpdate, setIsInstallingUpdate] = useState(false);
  const [updateErrorMessage, setUpdateErrorMessage] = useState("");
  const activeViewRef = useRef<AppView>(activeView);

  const [permissions, setPermissions] = useState<PermissionSnapshot | null>(null);
  const [permissionErrorMessage, setPermissionErrorMessage] = useState("");
  const [requestingPermission, setRequestingPermission] = useState<PermissionType | null>(null);
  const [isRefreshingPermissions, setIsRefreshingPermissions] = useState(false);
  const [usageStats, setUsageStats] = useState<UsageStatsReport | null>(null);
  const [usageStatsErrorMessage, setUsageStatsErrorMessage] = useState("");
  const [isRefreshingUsageStats, setIsRefreshingUsageStats] = useState(false);
  const [isResettingUsageStats, setIsResettingUsageStats] = useState(false);
  const [permissionCardDismissed, setPermissionCardDismissedState] = useState<boolean>(
    () => readPermissionCardDismissed()
  );
  const statusRef = useRef<AppStatus>("idle");

  useEffect(() => {
    let isMounted = true;

    async function loadOnboardingState() {
      try {
        const completed = await invoke<boolean>("get_onboarding_status");
        if (isMounted) {
          setOnboardingState(completed ? "completed" : "required");
        }
      } catch {
        if (isMounted) {
          setOnboardingState("completed");
        }
      }
    }

    void loadOnboardingState();
    return () => {
      isMounted = false;
    };
  }, []);

  useEffect(() => {
    statusRef.current = status;
  }, [status]);

  useEffect(() => {
    if (onboardingState !== "completed") return undefined;

    let isMounted = true;

    async function checkForUpdatesOnLaunch() {
      try {
        const update = await check();
        if (isMounted) {
          setAvailableUpdate(update);
        }
      } catch {
        // Keep launch resilient even if update checks fail.
      }
    }

    void checkForUpdatesOnLaunch();
    return () => {
      isMounted = false;
    };
  }, [onboardingState]);

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

  const refreshUsageStats = useCallback(async () => {
    setIsRefreshingUsageStats(true);
    try {
      const stats = await invoke<UsageStatsReport>("get_usage_stats");
      setUsageStats(stats);
      setUsageStatsErrorMessage("");
    } catch (error) {
      setUsageStatsErrorMessage(
        toErrorMessage(error, "Unable to load usage stats from the backend.")
      );
    } finally {
      setIsRefreshingUsageStats(false);
    }
  }, []);

  const resetUsageStats = useCallback(() => {
    if (isResettingUsageStats) return;
    if (!window.confirm("Reset all usage stats? This cannot be undone.")) return;

    void (async () => {
      setIsResettingUsageStats(true);
      setUsageStatsErrorMessage("");

      try {
        await invoke("reset_usage_stats");
        await refreshUsageStats();
      } catch (error) {
        setUsageStatsErrorMessage(toErrorMessage(error, "Failed to reset usage stats."));
      } finally {
        setIsResettingUsageStats(false);
      }
    })();
  }, [isResettingUsageStats, refreshUsageStats]);

  const dismissPermissionCard = useCallback(() => {
    setPermissionCardDismissedState(true);
    setPermissionCardDismissed(true);
  }, []);

  const installAvailableUpdate = useCallback(() => {
    if (!availableUpdate || isInstallingUpdate) return;

    void (async () => {
      setIsInstallingUpdate(true);
      setUpdateErrorMessage("");

      try {
        await availableUpdate.downloadAndInstall();
        setAvailableUpdate(null);
      } catch (error) {
        setUpdateErrorMessage(toErrorMessage(error, "Failed to install update."));
      } finally {
        setIsInstallingUpdate(false);
      }
    })();
  }, [availableUpdate, isInstallingUpdate]);

  useEffect(() => {
    activeViewRef.current = activeView;
  }, [activeView]);

  useEffect(() => {
    if (onboardingState !== "completed") return undefined;

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
        const initialUsageStats = await invoke<UsageStatsReport>("get_usage_stats");
        if (isMounted) {
          setUsageStats(initialUsageStats);
          setUsageStatsErrorMessage("");
        }
      } catch (error) {
        if (isMounted) {
          setUsageStatsErrorMessage(
            toErrorMessage(error, "Unable to load usage stats from the backend.")
          );
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
            if (activeViewRef.current === "dashboard") {
              void refreshUsageStats();
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
  }, [onboardingState, refreshUsageStats]);

  useEffect(() => {
    if (onboardingState !== "completed") return undefined;

    function handleWindowFocus() {
      void refreshPermissions();
      if (activeViewRef.current === "dashboard") {
        void refreshUsageStats();
      }
    }
    window.addEventListener("focus", handleWindowFocus);
    return () => window.removeEventListener("focus", handleWindowFocus);
  }, [onboardingState, refreshPermissions, refreshUsageStats]);

  useEffect(() => {
    if (onboardingState !== "completed") return;
    if (activeView !== "dashboard") return;
    void refreshUsageStats();
  }, [activeView, onboardingState, refreshUsageStats]);

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
        return "bg-emerald-500";
      case "transcribing":
        return "bg-amber-500";
      case "error":
        return "bg-destructive";
      default:
        return "bg-muted-foreground";
    }
  }, [status]);

  const handleOnboardingComplete = useCallback(() => {
    setOnboardingState("completed");
    void getCurrentWindow().hide().catch(() => {
      // Keep onboarding completion resilient if hide fails.
    });
  }, []);

  if (onboardingState === "loading") {
    return (
      <main className="flex h-screen items-center justify-center">
        <p className="text-sm text-muted-foreground">Loading Buzz...</p>
      </main>
    );
  }

  if (onboardingState === "required") {
    return <Onboarding onComplete={handleOnboardingComplete} />;
  }

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
              <p className="text-sm font-semibold text-sidebar-foreground tracking-tight">Buzz</p>
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
        <div className="flex min-w-0 flex-1 flex-col overflow-hidden">
          {/* Content header */}
          <header className="shrink-0 border-b px-4 py-3" data-tauri-drag-region="">
            <h1 className="text-sm font-semibold tracking-tight">
              {VIEW_TITLES[activeView]}
            </h1>
          </header>

          {availableUpdate && (
            <div className="shrink-0 border-b border-emerald-500/20 bg-emerald-50/60 px-4 py-2 dark:bg-emerald-950/20">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <p className="text-xs font-medium text-emerald-900 dark:text-emerald-100">
                  A new version of Buzz is available.
                </p>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={installAvailableUpdate}
                  disabled={isInstallingUpdate}
                >
                  {isInstallingUpdate ? "Updating..." : "Update Now"}
                </Button>
              </div>
              {updateErrorMessage && (
                <p className="mt-1 text-[11px] text-destructive">{updateErrorMessage}</p>
              )}
            </div>
          )}

          {/* Content body */}
          <div className="h-0 flex-1 overflow-y-auto">
            <div className="p-4">
              {activeView === "dashboard" && (
                <DashboardView
                  audioLevel={audioLevel}
                  isRefreshingPermissions={isRefreshingPermissions}
                  isRefreshingUsageStats={isRefreshingUsageStats}
                  isResettingUsageStats={isResettingUsageStats}
                  lastTranscript={lastTranscript}
                  onDismissPermissions={dismissPermissionCard}
                  onRefreshPermissions={() => void refreshPermissions()}
                  onRefreshUsageStats={() => void refreshUsageStats()}
                  onResetUsageStats={resetUsageStats}
                  onRequestPermission={requestPermission}
                  permissionErrorMessage={permissionErrorMessage}
                  permissions={permissions}
                  requestingPermission={requestingPermission}
                  showPermissionsCard={showPermissionsCard}
                  status={status}
                  statusDescription={statusDescription}
                  usageStats={usageStats}
                  usageStatsErrorMessage={usageStatsErrorMessage}
                />
              )}
              {activeView === "history" && (
                <HistoryPanel refreshSignal={historyRefreshSignal} />
              )}
              {activeView === "settings" && (
                <Settings />
              )}
            </div>
          </div>
        </div>
      </main>
    </TooltipProvider>
  );
}

export default App;
