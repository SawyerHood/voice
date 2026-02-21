export function clampAudioLevel(value: number): number {
  if (!Number.isFinite(value)) {
    return 0;
  }

  // Raw mic levels are typically 0.0–0.15 for normal speech.
  // Amplify with gain + sqrt curve so bars are visually responsive.
  const clamped = Math.max(0, Math.min(1, value));
  const gained = Math.min(1, clamped * 6);
  return Math.sqrt(gained);
}

export function pushAudioLevelHistory(
  history: number[],
  value: number,
  maxLength: number,
): number[] {
  const boundedLength = Math.max(1, Math.floor(maxLength));
  const normalized = clampAudioLevel(value);
  const next = history.slice(-(boundedLength - 1));
  next.push(normalized);

  while (next.length < boundedLength) {
    next.unshift(0);
  }

  return next;
}

export function formatElapsedLabel(elapsedMs: number): string {
  const safeMs = Number.isFinite(elapsedMs) ? Math.max(0, Math.floor(elapsedMs)) : 0;
  const totalSeconds = Math.floor(safeMs / 1000);
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;

  return `${minutes.toString().padStart(2, "0")}:${seconds.toString().padStart(2, "0")}`;
}
