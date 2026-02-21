import { describe, expect, it } from "vitest";

import { clampAudioLevel, formatElapsedLabel, pushAudioLevelHistory } from "./overlayUtils";

describe("clampAudioLevel", () => {
  it("bounds levels to 0..1", () => {
    expect(clampAudioLevel(-0.6)).toBe(0);
    expect(clampAudioLevel(0.35)).toBe(0.35);
    expect(clampAudioLevel(9)).toBe(1);
  });

  it("falls back to 0 for non-finite values", () => {
    expect(clampAudioLevel(Number.NaN)).toBe(0);
    expect(clampAudioLevel(Number.POSITIVE_INFINITY)).toBe(0);
  });
});

describe("pushAudioLevelHistory", () => {
  it("keeps a fixed-length rolling window", () => {
    expect(pushAudioLevelHistory([0, 0.2, 0.6], 1.4, 3)).toEqual([0.2, 0.6, 1]);
  });

  it("pads missing history with zeros", () => {
    expect(pushAudioLevelHistory([], 0.4, 4)).toEqual([0, 0, 0, 0.4]);
  });
});

describe("formatElapsedLabel", () => {
  it("formats elapsed milliseconds as mm:ss", () => {
    expect(formatElapsedLabel(0)).toBe("00:00");
    expect(formatElapsedLabel(65_000)).toBe("01:05");
    expect(formatElapsedLabel(9 * 60_000 + 7_999)).toBe("09:07");
  });
});
