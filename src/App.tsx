import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import HistoryPanel from "./HistoryPanel";
import Settings from "./Settings";
import "./App.css";

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

const TAB_LABEL: Record<AppTab, string> = {
  status: "Status",
  history: "History",
  settings: "Settings",
};

const PERMISSION_LABEL: Record<PermissionType, string> = {
  microphone: "Microphone",
  accessibility: "Accessibility",
};

const PERMISSION_HELP: Record<PermissionType, string> = {
  microphone: "Required to capture speech audio.",
  accessibility: "Required to type/paste into other apps.",
};

const PERMISSION_STATUS_LABEL: Record<PermissionState, string> = {
  granted: "Granted",
  denied: "Denied",
  not_determined: "Needs Access",
};

const PERMISSION_CARD_DISMISSED_KEY = "voice.permissionsOnboardingDismissed.v1";

function toErrorMessage(error: unknown, fallbackMessage: string): string {
  if (typeof error === "string" && error.trim()) {
    return error;
  }

  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }

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
    // Ignore storage write errors in restricted environments.
  }
}

function TabIcon({ tab }: { tab: AppTab }) {
  if (tab === "status") {
    return (
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 2.5a1 1 0 0 1 1 1v7.1l4.65 2.7a1 1 0 1 1-1 1.74l-5.15-3A1 1 0 0 1 11 11V3.5a1 1 0 0 1 1-1Z" />
        <path d="M12 22a10 10 0 1 1 10-10 1 1 0 1 1-2 0 8 8 0 1 0-8 8 1 1 0 1 1 0 2Z" />
      </svg>
    );
  }

  if (tab === "history") {
    return (
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 3a9 9 0 1 1-8.95 10h2.05a7 7 0 1 0 1.97-5.09L9 10H3V4l2.63 2.63A8.95 8.95 0 0 1 12 3Z" />
        <path d="M12 8a1 1 0 0 1 1 1v3.4l2.8 1.62a1 1 0 1 1-1 1.73l-3.3-1.9A1 1 0 0 1 11 13V9a1 1 0 0 1 1-1Z" />
      </svg>
    );
  }

  return (
    <svg viewBox="0 0 24 24" aria-hidden="true">
      <path d="M11.3 2.15a1 1 0 0 1 1.4 0l1.03 1.03a1 1 0 0 0 .86.27l1.43-.31a1 1 0 0 1 1.19.8l.27 1.42a1 1 0 0 0 .56.72l1.31.64a1 1 0 0 1 .46 1.34l-.64 1.31a1 1 0 0 0 0 .9l.64 1.31a1 1 0 0 1-.46 1.34l-1.31.64a1 1 0 0 0-.56.72l-.27 1.42a1 1 0 0 1-1.19.8l-1.43-.31a1 1 0 0 0-.86.27L12.7 21.85a1 1 0 0 1-1.4 0l-1.03-1.03a1 1 0 0 0-.86-.27l-1.43.31a1 1 0 0 1-1.19-.8l-.27-1.42a1 1 0 0 0-.56-.72l-1.31-.64a1 1 0 0 1-.46-1.34l.64-1.31a1 1 0 0 0 0-.9l-.64-1.31a1 1 0 0 1 .46-1.34l1.31-.64a1 1 0 0 0 .56-.72l.27-1.42a1 1 0 0 1 1.19-.8l1.43.31a1 1 0 0 0 .86-.27Z" />
      <path d="M12 15.5A3.5 3.5 0 1 1 12 8.5a3.5 3.5 0 0 1 0 7Zm0-2A1.5 1.5 0 1 0 12 10.5a1.5 1.5 0 0 0 0 3Z" />
    </svg>
  );
}

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
    <section className="permissions-card" aria-live="polite">
      <div className="permissions-header">
        <div>
          <p className="card-title">Permissions</p>
          <p className="permissions-description">
            {permissions?.allGranted
              ? "All required permissions are granted."
              : "Voice needs these macOS permissions to record and insert text."}
          </p>
        </div>

        {showDismiss ? (
          <button type="button" className="utility-button" onClick={onDismiss}>
            Dismiss
          </button>
        ) : null}
      </div>

      {errorMessage ? <p className="permissions-error">{errorMessage}</p> : null}

      <div className="permissions-list">
        {permissionOrder.map((permissionType) => {
          const state = permissions?.[permissionType] ?? "not_determined";
          const isGranted = state === "granted";
          const isRequesting = requestingPermission === permissionType;

          return (
            <div className="permission-row" key={permissionType}>
              <div>
                <p className="permission-name">{PERMISSION_LABEL[permissionType]}</p>
                <p className="permission-help">{PERMISSION_HELP[permissionType]}</p>
              </div>

              <div className="permission-row-actions">
                <span className={`permission-state permission-${state}`}>
                  {PERMISSION_STATUS_LABEL[state]}
                </span>
                <button
                  type="button"
                  className="secondary-button permission-action-button"
                  disabled={isGranted || isRequesting}
                  onClick={() => onRequestPermission(permissionType)}
                >
                  {isGranted ? "Granted" : isRequesting ? "Requesting..." : "Grant Access"}
                </button>
              </div>
            </div>
          );
        })}
      </div>

      <div className="permissions-footer">
        <button
          type="button"
          className="utility-button"
          onClick={onRefresh}
          disabled={isRefreshing || requestingPermission !== null}
        >
          {isRefreshing ? "Refreshing..." : "Refresh Status"}
        </button>
      </div>
    </section>
  );
}

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
  return (
    <div className="status-layout">
      {showPermissionsCard ? (
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
      ) : null}

      <section className={`status-card status-${status}`}>
        <p className="status-label">{STATUS_LABEL[status]}</p>
        <p className="status-description">{statusDescription}</p>
      </section>

      <section className="audio-level-card">
        <p className="card-title">Audio Level</p>
        <div className="audio-meter-track">
          <div
            className={`audio-meter-fill ${status === "listening" ? "active" : ""}`}
            style={{ width: `${Math.round(audioLevel * 100)}%` }}
          />
        </div>
        <p className="audio-meter-value">
          {status === "listening"
            ? `${Math.round(audioLevel * 100)}% input`
            : "Waiting for recording"}
        </p>
      </section>

      <section className="transcript-card">
        <p className="card-title">Last Transcript</p>
        <p className={lastTranscript ? "transcript-value" : "transcript-placeholder"}>
          {lastTranscript || "No transcript captured yet."}
        </p>
      </section>
    </div>
  );
}

function App() {
  const [status, setStatus] = useState<AppStatus>("idle");
  const [activeTab, setActiveTab] = useState<AppTab>("status");
  const [errorMessage, setErrorMessage] = useState("");
  const [audioLevel, setAudioLevel] = useState(0);
  const [lastTranscript, setLastTranscript] = useState("");
  const [backendSynced, setBackendSynced] = useState<boolean>(true);

  const [permissions, setPermissions] = useState<PermissionSnapshot | null>(null);
  const [permissionErrorMessage, setPermissionErrorMessage] = useState("");
  const [requestingPermission, setRequestingPermission] = useState<PermissionType | null>(null);
  const [isRefreshingPermissions, setIsRefreshingPermissions] = useState(false);
  const [permissionCardDismissed, setPermissionCardDismissedState] = useState<boolean>(
    () => readPermissionCardDismissed(),
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
        toErrorMessage(error, "Unable to load permission status from the backend."),
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
        toErrorMessage(error, "Unable to request macOS permission from the backend."),
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
    let isMounted = true;
    let unlistenFns: UnlistenFn[] = [];

    async function bindBackend() {
      try {
        const [initialStatus, initialAudioLevel, initialPermissions] = await Promise.all([
          invoke<AppStatus>("get_status"),
          invoke<number>("get_audio_level"),
          invoke<PermissionSnapshot>("check_permissions"),
        ]);

        if (!isMounted) {
          return;
        }

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
            if (payload !== "error") {
              setErrorMessage("");
            }
          }),
          listen<number>("audio-level", ({ payload }) => {
            const normalized = Math.max(0, Math.min(1, Number(payload) || 0));
            if (statusRef.current !== "listening" && normalized > 0) {
              return;
            }

            const quantized = Math.round(normalized * 100) / 100;
            setAudioLevel((previous) =>
              Math.abs(previous - quantized) < 0.01 ? previous : quantized,
            );
          }),
          listen<TranscriptReadyEvent>("voice://transcript-ready", ({ payload }) => {
            setLastTranscript(payload.text ?? "");
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
        if (isMounted) {
          setBackendSynced(false);
        }
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
    return () => {
      window.removeEventListener("focus", handleWindowFocus);
    };
  }, [refreshPermissions]);

  const hasMissingPermissions = useMemo(
    () => !permissions || !permissions.allGranted,
    [permissions],
  );

  const showPermissionsCard = useMemo(() => {
    if (!backendSynced) {
      return false;
    }

    return hasMissingPermissions || !permissionCardDismissed;
  }, [backendSynced, hasMissingPermissions, permissionCardDismissed]);

  const statusDescription = useMemo(() => {
    switch (status) {
      case "idle":
        return "Waiting for the global hotkey.";
      case "listening":
        return "Capturing microphone input.";
      case "transcribing":
        return "Converting audio to text.";
      case "error":
        return errorMessage || "A recoverable issue occurred.";
      default:
        return "Unknown state.";
    }
  }, [errorMessage, status]);

  return (
    <main className="app-shell">
      <header className="app-header">
        <div>
          <p className="eyebrow">Voice Utility</p>
          <h1>{TAB_LABEL[activeTab]}</h1>
        </div>
      </header>

      <nav className="app-nav" aria-label="Primary">
        {(["status", "history", "settings"] as const).map((tab) => (
          <button
            key={tab}
            type="button"
            className={`nav-button ${activeTab === tab ? "active" : ""}`}
            onClick={() => setActiveTab(tab)}
            aria-current={activeTab === tab ? "page" : undefined}
          >
            <TabIcon tab={tab} />
            <span>{TAB_LABEL[tab]}</span>
          </button>
        ))}
      </nav>

      <section className="tab-content">
        {activeTab === "status" ? (
          <StatusView
            audioLevel={audioLevel}
            isRefreshingPermissions={isRefreshingPermissions}
            lastTranscript={lastTranscript}
            onDismissPermissions={dismissPermissionCard}
            onRefreshPermissions={() => {
              void refreshPermissions();
            }}
            onRequestPermission={requestPermission}
            permissionErrorMessage={permissionErrorMessage}
            permissions={permissions}
            requestingPermission={requestingPermission}
            showPermissionsCard={showPermissionsCard}
            status={status}
            statusDescription={statusDescription}
          />
        ) : null}
        {activeTab === "history" ? <HistoryPanel /> : null}
        {activeTab === "settings" ? <Settings /> : null}
      </section>

      <p className="backend-sync">
        Backend sync: {backendSynced ? "connected" : "frontend-only fallback"}
      </p>
    </main>
  );
}

export default App;
