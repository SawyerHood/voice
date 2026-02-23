export type OnboardingPracticeStatus = "idle" | "listening" | "transcribing" | "error";

function normalizeTranscriptText(value: unknown): string {
  if (typeof value !== "string") {
    return "";
  }
  return value.trim();
}

export function extractTranscriptText(payload: unknown): string {
  if (typeof payload === "string") {
    return payload.trim();
  }

  if (!payload || typeof payload !== "object") {
    return "";
  }

  const eventPayload = payload as Record<string, unknown>;

  const fromText = normalizeTranscriptText(eventPayload.text);
  if (fromText.length > 0) {
    return fromText;
  }

  return normalizeTranscriptText(eventPayload.transcript);
}

export function practiceStatusLabel(status: OnboardingPracticeStatus): string {
  switch (status) {
    case "listening":
      return "Recording";
    case "transcribing":
      return "Transcribing";
    case "error":
      return "Error";
    default:
      return "Ready";
  }
}
