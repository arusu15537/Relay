import { create } from "zustand";
import type * as T from "./types";

export interface RunView {
  runId: string;
  profileId: string;
  profileName: string;
  status: T.PaneStatus;
  stream: string;
  reasoning: string;
  callLabel: string;
  liveTps: number | null;
  tokens: number;
  notes: string[];
  response: T.ImplementResponse | null;
  error: string | null;
  decisions: Record<string, "pending" | "accepted" | "rejected">;
  edited: Record<string, string>;
  review: T.Review | null;
  reviewStatus: T.PaneStatus;
  reviewStream: string;
  reviewJobId: string;
  startedAt: number;
  spec: string;
  fromHistory: boolean;
}

export interface Toast {
  id: number;
  kind: "info" | "error" | "ok";
  text: string;
}

export interface ModalButton {
  id: string;
  label: string;
  kind?: "primary" | "danger" | "default";
}

export interface ModalState {
  title: string;
  body: string;
  details?: string[];
  buttons: ModalButton[];
  resolve: (id: string) => void;
}

export interface Store {
  ready: boolean;
  modal: ModalState | null;
  boot: T.AppStateDto | null;
  settings: T.Settings | null;
  profiles: T.Profile[];
  server: T.ServerStatus | null;
  gpu: [number, number] | null;
  log: string[];
  downloads: Record<string, T.DownloadProgress>;

  root: string;
  files: T.FileEntry[];
  filesTruncated: boolean;
  checked: string[];
  /** Context files to send in full instead of as an outline (this session only). */
  fullContext: string[];
  git: T.GitStatus | null;

  jobId: string;
  instruction: string;
  expandStatus: T.PaneStatus;
  claudeStream: string;
  claudeThinking: string;
  claudeActivity: string[];
  expandInfo: string;
  expandError: string | null;

  spec: string;
  specStatus: T.PaneStatus;
  prepared: T.Prepared | null;
  /** Last spec-review prewarm of the local model's prompt cache. */
  prewarm: T.PrewarmResult | null;
  implementProfile: string;
  mode: "auto" | T.SplitMode;

  runs: RunView[];
  activeRun: string | null;
  compareWith: string | null;

  settingsOpen: boolean;
  historyOpen: boolean;
  logOpen: boolean;
  toasts: Toast[];
}

export const useStore = create<Store>(() => ({
  ready: false,
  modal: null,
  boot: null,
  settings: null,
  profiles: [],
  server: null,
  gpu: null,
  log: [],
  downloads: {},
  root: "",
  files: [],
  filesTruncated: false,
  checked: [],
  fullContext: [],
  git: null,
  jobId: crypto.randomUUID(),
  instruction: "",
  expandStatus: "idle",
  claudeStream: "",
  claudeThinking: "",
  claudeActivity: [],
  expandInfo: "",
  expandError: null,
  spec: "",
  specStatus: "idle",
  prepared: null,
  prewarm: null,
  implementProfile: "",
  mode: "auto",
  runs: [],
  activeRun: null,
  compareWith: null,
  settingsOpen: false,
  historyOpen: false,
  logOpen: false,
  toasts: [],
}));

export const set = useStore.setState;
export const get = useStore.getState;

let toastId = 0;
export function toast(text: string, kind: Toast["kind"] = "info") {
  const id = ++toastId;
  set((s) => ({ toasts: [...s.toasts, { id, kind, text }] }));
  setTimeout(() => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })), kind === "error" ? 9000 : 4500);
}

export function updateRun(runId: string, f: (r: RunView) => Partial<RunView>) {
  set((s) => ({ runs: s.runs.map((r) => (r.runId === runId ? { ...r, ...f(r) } : r)) }));
}

export function profileName(id: string): string {
  return get().profiles.find((p) => p.id === id)?.name ?? id;
}

/** Show a modal and resolve with the id of the button pressed ("cancel" on Escape). */
export function ask(title: string, body: string, buttons: ModalButton[], details?: string[]): Promise<string> {
  return new Promise((resolve) => {
    set({
      modal: {
        title,
        body,
        details,
        buttons,
        resolve: (id: string) => {
          set({ modal: null });
          resolve(id);
        },
      },
    });
  });
}
