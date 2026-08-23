import type { ConnectionState } from "./stream";

export type StreamPresentationState =
  | "connecting"
  | "live"
  | "reconnecting"
  | "recovering"
  | "paused-live"
  | "resetting"
  | "warning"
  | "failed";

export type StreamPresentation = {
  state: StreamPresentationState;
  label: string;
  tone: "good" | "warning" | "error";
};

export type StreamPresentationInput = {
  connectionState: ConnectionState;
  paused: boolean;
  resetting: boolean;
  warning: boolean;
};

const presentations: Record<StreamPresentationState, StreamPresentation> = {
  connecting: { state: "connecting", label: "connecting", tone: "warning" },
  live: { state: "live", label: "live", tone: "good" },
  reconnecting: { state: "reconnecting", label: "reconnecting", tone: "warning" },
  recovering: { state: "recovering", label: "recovering", tone: "warning" },
  "paused-live": {
    state: "paused-live",
    label: "paused · stream live",
    tone: "good"
  },
  resetting: {
    state: "resetting",
    label: "resetting projection",
    tone: "warning"
  },
  warning: { state: "warning", label: "stream warning", tone: "warning" },
  failed: {
    state: "failed",
    label: "stream error · refresh required",
    tone: "error"
  }
};

export function deriveStreamPresentation(
  input: StreamPresentationInput
): StreamPresentation {
  let state: StreamPresentationState;
  if (input.connectionState === "failed") state = "failed";
  else if (input.warning) state = "warning";
  else if (input.connectionState === "recovering") state = "recovering";
  else if (input.connectionState === "reconnecting") state = "reconnecting";
  else if (input.resetting) state = "resetting";
  else if (input.connectionState === "connecting") state = "connecting";
  else state = input.paused ? "paused-live" : "live";
  return presentations[state];
}

export function streamStripLabel(presentation: StreamPresentation): string {
  return presentation.state === "live" || presentation.state === "paused-live"
    ? "stream live"
    : presentation.label;
}
