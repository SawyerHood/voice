import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { type CSSProperties, useEffect, useRef, useState } from "react";
import { clampAudioLevel, formatElapsedLabel, pushAudioLevelHistory } from "./overlayUtils";
import "./Overlay.css";

type AppStatus = "idle" | "listening" | "transcribing" | "error";

const BAR_COUNT = 22;
const SMOOTHING_FACTOR = 0.35;
const EVENT_STATUS_CHANGED = "voice://status-changed";
const EVENT_OVERLAY_AUDIO_LEVEL = "voice://overlay-audio-level";

function emptyHistory(): number[] {
  return Array.from({ length: BAR_COUNT }, () => 0);
}

function Overlay() {
  const [status, setStatus] = useState<AppStatus>("idle");
  const [elapsedMs, setElapsedMs] = useState(0);
  const [audioHistory, setAudioHistory] = useState<number[]>(() => emptyHistory());
  const statusRef = useRef<AppStatus>("idle");
  const startedAtRef = useRef<number | null>(null);
  const smoothedHistoryRef = useRef<number[]>(emptyHistory());

  useEffect(() => {
    let isMounted = true;
    let unlistenFns: UnlistenFn[] = [];

    const resetHistory = () => {
      const empty = emptyHistory();
      smoothedHistoryRef.current = empty;
      setAudioHistory(empty);
    };

    const pushSmoothedLevel = (rawLevel: number) => {
      const normalized = clampAudioLevel(rawLevel);
      const previousHistory = smoothedHistoryRef.current;
      const previousLevel = previousHistory[previousHistory.length - 1] ?? 0;
      const smoothedLevel = previousLevel + (normalized - previousLevel) * SMOOTHING_FACTOR;
      const nextHistory = pushAudioLevelHistory(previousHistory, smoothedLevel, BAR_COUNT);

      smoothedHistoryRef.current = nextHistory;
      setAudioHistory(nextHistory);
    };

    const applyStatus = (nextStatus: AppStatus) => {
      const previousStatus = statusRef.current;
      statusRef.current = nextStatus;
      setStatus(nextStatus);

      if (nextStatus === "listening") {
        if (previousStatus !== "listening") {
          resetHistory();
        }

        if (startedAtRef.current === null) {
          startedAtRef.current = Date.now();
          setElapsedMs(0);
        }
        return;
      }

      if (nextStatus === "transcribing") {
        if (startedAtRef.current !== null) {
          setElapsedMs(Date.now() - startedAtRef.current);
        }
        resetHistory();
        return;
      }

      startedAtRef.current = null;
      setElapsedMs(0);
      resetHistory();
    };

    async function bindOverlayEvents() {
      try {
        const [initialStatus, initialAudioLevel] = await Promise.all([
          invoke<AppStatus>("get_status"),
          invoke<number>("get_audio_level"),
        ]);

        if (!isMounted) {
          return;
        }

        applyStatus(initialStatus);
        if (initialStatus === "listening") {
          pushSmoothedLevel(initialAudioLevel);
        }
      } catch {
        // Overlay remains passive if backend sync is unavailable.
      }

      try {
        const listeners = await Promise.all([
          listen<AppStatus>(EVENT_STATUS_CHANGED, ({ payload }) => {
            applyStatus(payload);
          }),
          listen<number>(EVENT_OVERLAY_AUDIO_LEVEL, ({ payload }) => {
            if (statusRef.current !== "listening") {
              return;
            }

            pushSmoothedLevel(payload);
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

  return (
    <main className="overlay-root">
      <section
        className={`overlay-pill ${isListening ? "active" : ""} ${
          isTranscribing ? "transcribing" : ""
        }`}
      >
        <span className="recording-indicator" aria-hidden="true">
          <span className="recording-dot" />
        </span>

        {isTranscribing ? (
          <div className="overlay-loading" role="status" aria-live="polite">
            <span className="overlay-loading-label">Transcribing</span>
            <span className="overlay-loading-dots" aria-hidden="true">
              <span />
              <span />
              <span />
            </span>
          </div>
        ) : (
          <div className="overlay-waveform" aria-hidden="true">
            {audioHistory.map((level, index) => (
              <span
                key={index}
                className="overlay-waveform-bar"
                style={{ "--level": level } as CSSProperties}
              />
            ))}
          </div>
        )}

        <p className="overlay-elapsed">{formatElapsedLabel(elapsedMs)}</p>
      </section>
    </main>
  );
}

export default Overlay;
