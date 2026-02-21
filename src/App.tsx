import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import Settings from "./Settings";
import "./App.css";

type AppStatus = "idle" | "listening" | "transcribing" | "error";
type AppView = "status" | "settings";
type TranscriptReadyEvent = { text: string };
type PipelineErrorEvent = { stage: string; message: string };

const STATUS_LABEL: Record<AppStatus, string> = {
  idle: "Idle",
  listening: "Listening",
  transcribing: "Transcribing",
  error: "Error",
};

function App() {
  const [activeView, setActiveView] = useState<AppView>("status");
  const [status, setStatus] = useState<AppStatus>("idle");
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
          <h1>{activeView === "status" ? "Status" : "Settings"}</h1>
        </div>
        <nav className="view-nav" aria-label="View navigation">
          <button
            type="button"
            className={`view-nav-button ${activeView === "status" ? "active" : ""}`}
            onClick={() => setActiveView("status")}
          >
            Status
          </button>
          <button
            type="button"
            className={`view-nav-button ${activeView === "settings" ? "active" : ""}`}
            onClick={() => setActiveView("settings")}
          >
            Settings
          </button>
        </nav>
      </header>

      {activeView === "status" ? (
        <>
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

          <p className="backend-sync">
            Backend sync: {backendSynced ? "connected" : "frontend-only fallback"}
          </p>
        </>
      ) : (
        <Settings />
      )}
    </main>
  );
}

export default App;
