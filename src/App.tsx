import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import HistoryPanel from "./HistoryPanel";
import Settings from "./Settings";
import "./App.css";

type AppStatus = "idle" | "listening" | "transcribing" | "error";
type AppTab = "status" | "history" | "settings";
type TranscriptReadyEvent = { text: string };
type PipelineErrorEvent = { stage: string; message: string };

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

type StatusViewProps = {
  audioLevel: number;
  lastTranscript: string;
  status: AppStatus;
  statusDescription: string;
};

function StatusView({ audioLevel, lastTranscript, status, statusDescription }: StatusViewProps) {
  return (
    <div className="status-layout">
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

  useEffect(() => {
    let isMounted = true;
    let unlistenFns: UnlistenFn[] = [];

    async function bindBackend() {
      try {
        const [initialStatus, initialAudioLevel] = await Promise.all([
          invoke<AppStatus>("get_status"),
          invoke<number>("get_audio_level"),
        ]);

        if (!isMounted) {
          return;
        }

        setStatus(initialStatus);
        setAudioLevel(Number.isFinite(initialAudioLevel) ? initialAudioLevel : 0);
      } catch {
        if (isMounted) {
          setBackendSynced(false);
        }
      }

      try {
        const listeners = await Promise.all([
          listen<AppStatus>("voice://status-changed", ({ payload }) => {
            setStatus(payload);
            if (payload !== "error") {
              setErrorMessage("");
            }
          }),
          listen<number>("audio-level", ({ payload }) => {
            const normalized = Math.max(0, Math.min(1, Number(payload) || 0));
            setAudioLevel(normalized);
          }),
          listen<TranscriptReadyEvent>("voice://transcript-ready", ({ payload }) => {
            setLastTranscript(payload.text ?? "");
          }),
          listen<PipelineErrorEvent>("voice://pipeline-error", ({ payload }) => {
            setErrorMessage(payload.message || "An unexpected pipeline error occurred.");
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
            lastTranscript={lastTranscript}
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
