import { describe, expect, it } from "vitest";
import {
  deriveStreamPresentation,
  streamStripLabel,
  type StreamPresentationInput,
  type StreamPresentationState
} from "./presentation";

type Case = StreamPresentationInput & {
  state: StreamPresentationState;
  label: string;
  stripLabel?: string;
};

const cases: Case[] = [
  {
    connectionState: "connecting",
    paused: false,
    resetting: false,
    warning: false,
    state: "connecting",
    label: "connecting"
  },
  {
    connectionState: "live",
    paused: false,
    resetting: false,
    warning: false,
    state: "live",
    label: "live",
    stripLabel: "stream live"
  },
  {
    connectionState: "reconnecting",
    paused: false,
    resetting: false,
    warning: false,
    state: "reconnecting",
    label: "reconnecting"
  },
  {
    connectionState: "recovering",
    paused: false,
    resetting: false,
    warning: false,
    state: "recovering",
    label: "recovering"
  },
  {
    connectionState: "live",
    paused: true,
    resetting: false,
    warning: false,
    state: "paused-live",
    label: "paused · stream live",
    stripLabel: "stream live"
  },
  {
    connectionState: "live",
    paused: false,
    resetting: true,
    warning: false,
    state: "resetting",
    label: "resetting projection"
  },
  {
    connectionState: "live",
    paused: false,
    resetting: false,
    warning: true,
    state: "warning",
    label: "stream warning"
  },
  {
    connectionState: "failed",
    paused: false,
    resetting: false,
    warning: false,
    state: "failed",
    label: "stream error · refresh required"
  }
];

describe("deriveStreamPresentation", () => {
  for (const item of cases) {
    it(`maps ${item.state} to stable visible wording`, () => {
      const presentation = deriveStreamPresentation(item);
      expect(presentation).toMatchObject({ state: item.state, label: item.label });
      expect(streamStripLabel(presentation)).toBe(item.stripLabel ?? item.label);
    });
  }

  it.each([
    ["paused reconnect", "reconnecting", true, true, false, "reconnecting"],
    ["paused recovery", "recovering", true, true, false, "recovering"],
    ["paused reset", "live", true, true, false, "resetting"],
    ["warning over reset", "live", true, true, true, "warning"],
    ["failure over warning", "failed", true, true, true, "failed"]
  ] as const)(
    "%s resolves to %s",
    (_name, connectionState, paused, resetting, warning, state) => {
      expect(
        deriveStreamPresentation({ connectionState, paused, resetting, warning }).state
      ).toBe(state);
    }
  );
});
