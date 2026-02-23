import { describe, expect, it } from "vitest";

import { extractTranscriptText, practiceStatusLabel } from "./onboardingUtils";

describe("onboardingUtils", () => {
  it("extracts transcript text from known payload shapes", () => {
    expect(extractTranscriptText("  hello world  ")).toBe("hello world");
    expect(extractTranscriptText({ text: "  from-text-field  " })).toBe("from-text-field");
    expect(extractTranscriptText({ transcript: "  from-transcript-field  " })).toBe(
      "from-transcript-field"
    );
  });

  it("returns an empty string when transcript payload is missing", () => {
    expect(extractTranscriptText({})).toBe("");
    expect(extractTranscriptText({ text: "   " })).toBe("");
    expect(extractTranscriptText(null)).toBe("");
  });

  it("maps onboarding practice statuses to display labels", () => {
    expect(practiceStatusLabel("idle")).toBe("Ready");
    expect(practiceStatusLabel("listening")).toBe("Recording");
    expect(practiceStatusLabel("transcribing")).toBe("Transcribing");
    expect(practiceStatusLabel("error")).toBe("Error");
  });
});
