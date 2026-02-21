import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type AppStatus = "idle" | "listening" | "transcribing" | "error";

const STATUS_LABEL: Record<AppStatus, string> = {
  idle: "Idle",
  listening: "Listening",
  transcribing: "Transcribing",
  error: "Error",
};

function App() {
  const [status, setStatus] = useState<AppStatus>("idle");
  const [errorMessage, setErrorMessage] = useState<string>("");
  const [backendSynced, setBackendSynced] = useState<boolean>(true);

  useEffect(() => {
    void invoke<AppStatus>("get_status")
      .then((initialStatus) => {
        setStatus(initialStatus);
      })
      .catch(() => {
        setBackendSynced(false);
      });
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

  function updateStatus(next: AppStatus) {
    setStatus(next);
    if (next !== "error") {
      setErrorMessage("");
    }

    void invoke("set_status", { status: next }).catch(() => {
      setBackendSynced(false);
    });
  }

  function simulateError() {
    setErrorMessage("Provider unavailable (stub)");
    updateStatus("error");
  }

  return (
    <main className="app-shell">
      <header>
        <p className="eyebrow">Voice Utility</p>
        <h1>Status</h1>
      </header>

      <section className={`status-card status-${status}`}>
        <p className="status-label">{STATUS_LABEL[status]}</p>
        <p className="status-description">{statusDescription}</p>
      </section>

      <section className="controls">
        <button type="button" onClick={() => updateStatus("idle")}>
          Set Idle
        </button>
        <button type="button" onClick={() => updateStatus("listening")}>
          Set Listening
        </button>
        <button type="button" onClick={() => updateStatus("transcribing")}>
          Set Transcribing
        </button>
        <button type="button" onClick={simulateError}>
          Set Error
        </button>
      </section>

      <p className="backend-sync">
        Backend sync: {backendSynced ? "connected" : "frontend-only fallback"}
      </p>
    </main>
  );
}

export default App;
