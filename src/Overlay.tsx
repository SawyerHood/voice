import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useRef, useState } from "react";
import { formatElapsedLabel } from "./overlayUtils";
import "./Overlay.css";

type AppStatus = "idle" | "listening" | "transcribing" | "error";

const EVENT_STATUS_CHANGED = "voice://status-changed";
const COMMAND_CANCEL_RECORDING = "cancel_recording";

function Overlay() {
  const [status, setStatus] = useState<AppStatus>("idle");
  const [elapsedMs, setElapsedMs] = useState(0);
  const statusRef = useRef<AppStatus>("idle");
  const startedAtRef = useRef<number | null>(null);
  const cancelInFlightRef = useRef(false);

  useEffect(() => {
    let isMounted = true;
    let unlistenFns: UnlistenFn[] = [];

    const applyStatus = (nextStatus: AppStatus) => {
      const previousStatus = statusRef.current;
      statusRef.current = nextStatus;
      setStatus(nextStatus);

      if (nextStatus === "listening") {
        if (previousStatus !== "listening") {
          startedAtRef.current = Date.now();
          setElapsedMs(0);
        } else if (startedAtRef.current === null) {
          startedAtRef.current = Date.now();
          setElapsedMs(0);
        }

        return;
      }

      if (nextStatus === "transcribing") {
        if (startedAtRef.current !== null) {
          setElapsedMs(Date.now() - startedAtRef.current);
          startedAtRef.current = null;
        }

        return;
      }

      startedAtRef.current = null;
      setElapsedMs(0);
    };

    async function bindOverlayEvents() {
      try {
        const initialStatus = await invoke<AppStatus>("get_status");

        if (!isMounted) {
          return;
        }

        applyStatus(initialStatus);
      } catch {
        // Overlay remains passive if backend sync is unavailable.
      }

      try {
        const listeners = await Promise.all([
          listen<AppStatus>(EVENT_STATUS_CHANGED, ({ payload }) => {
            applyStatus(payload);
          }),
        ]);

        if (!isMounted) {
          listeners.forEach((dispose) => dispose());
          return;
        }

        unlistenFns = listeners;
      } catch {
        // Overlay remains passive if backend listeners are unavailable.
      }
    }

    void bindOverlayEvents();

    return () => {
      isMounted = false;
      unlistenFns.forEach((dispose) => dispose());
    };
  }, []);

  useEffect(() => {
    if (status !== "listening") {
      return;
    }

    const interval = window.setInterval(() => {
      if (startedAtRef.current === null) {
        return;
      }

      setElapsedMs(Date.now() - startedAtRef.current);
    }, 100);

    return () => {
      window.clearInterval(interval);
    };
  }, [status]);

  const isListening = status === "listening";
  const isTranscribing = status === "transcribing";
  const canCancel = isListening || isTranscribing;
  const statusLabel = isListening ? "Listening..." : isTranscribing ? "Transcribing..." : "";

  const handleCancel = () => {
    if (cancelInFlightRef.current) {
      return;
    }

    cancelInFlightRef.current = true;
    void invoke(COMMAND_CANCEL_RECORDING).finally(() => {
      cancelInFlightRef.current = false;
    });
  };

  return (
    <main className="overlay-root">
      <section
        className={`overlay-pill ${isListening ? "active" : ""} ${
          isTranscribing ? "transcribing" : ""
        }`}
      >
        {canCancel ? (
          <button
            type="button"
            className="overlay-cancel-button"
            onClick={handleCancel}
            aria-label="Cancel recording"
          >
            ×
          </button>
        ) : null}
        <span className="recording-indicator" aria-hidden="true">
          <span className="recording-dot" />
        </span>
        <p className="overlay-transcript-text" aria-live="polite">{statusLabel}</p>
        <p className="overlay-elapsed">{isListening ? formatElapsedLabel(elapsedMs) : "..."}</p>
      </section>
    </main>
  );
}

export default Overlay;
