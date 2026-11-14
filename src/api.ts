import { invoke } from "@tauri-apps/api/core";
import type * as T from "./types";

export const api = {
  getAppState: () => invoke<T.AppStateDto>("get_app_state"),
  saveSettings: (settings: T.Settings) => invoke<void>("save_settings", { settings }),
  saveProfiles: (profiles: T.Profile[]) => invoke<T.Profile[]>("save_profiles", { profiles }),
  resetProfile: (id: string) => invoke<T.Profile>("reset_profile", { id }),
  switchProfile: (id: string) => invoke<T.ServerStatus>("switch_profile", { id }),
  stopServer: () => invoke<void>("stop_server"),
  serverStatus: () => invoke<T.ServerStatus>("server_status"),
  serverLog: () => invoke<string[]>("server_log"),
  gpuMemory: () => invoke<[number, number] | null>("gpu_memory"),
  testProfile: (id: string) => invoke<T.ProfileTest>("test_profile", { id }),
  testClaude: () => invoke<string>("test_claude"),
  listFiles: (root: string) => invoke<T.FileList>("list_files", { root }),
  readProjectFile: (root: string, path: string) => invoke<string | null>("read_project_file", { root, path }),
  gitStatus: (root: string) => invoke<T.GitStatus>("git_status", { root }),
  gitCheckpoint: (root: string, message: string) => invoke<string>("git_checkpoint", { root, message }),
  expand: (req: { jobId: string; instruction: string; projectRoot: string; contextFiles: string[]; profileId: string }) =>
    invoke<T.ExpandResult>("expand", { req }),
  parseSpec: (spec: string) => invoke<T.ParsedSpec>("parse_spec_cmd", { spec }),
  prepareImplement: (root: string, spec: string, contextFiles: string[], fullContext: string[], profileId: string) =>
    invoke<T.Prepared>("prepare_implement", { root, spec, contextFiles, fullContext, profileId }),
  prewarm: (cmd: {
    projectRoot: string;
    spec: string;
    contextFiles: string[];
    fullContext: string[];
    profileId: string;
    mode: T.SplitMode | null;
  }) => invoke<T.PrewarmResult>("prewarm", { cmd }),
  implement: (cmd: {
    jobId: string;
    runId: string;
    projectRoot: string;
    instruction: string;
    spec: string;
    contextFiles: string[];
    mode: T.SplitMode | null;
    profileId: string;
    fullContext: string[];
  }) => invoke<T.ImplementResponse>("implement", { cmd }),
  cancelJob: (id: string) => invoke<boolean>("cancel_job", { id }),
  acceptFile: (runId: string, path: string, content: string | null, force: boolean) =>
    invoke<T.AcceptOutcome>("accept_file", { runId, path, content, force }),
  rejectFile: (runId: string, path: string) => invoke<void>("reject_file", { runId, path }),
  reviewRun: (runId: string, jobId: string) => invoke<T.Review>("review_run", { runId, jobId }),
  historyList: (query: string) => invoke<T.JobSummary[]>("history_list", { query }),
  historyGet: (id: string) => invoke<T.JobDetail | null>("history_get", { id }),
  historyDelete: (id: string) => invoke<void>("history_delete", { id }),
  templatesList: () => invoke<T.TemplateInfo[]>("templates_list"),
  templateSave: (name: string, text: string) => invoke<void>("template_save", { name, text }),
  templateReset: (name: string) => invoke<string>("template_reset", { name }),
  setApiKey: (value: string) => invoke<void>("set_api_key", { value }),
  setEndpointKey: (profileId: string, value: string) => invoke<void>("set_endpoint_key", { profileId, value }),
  hasEndpointKey: (profileId: string) => invoke<boolean>("has_endpoint_key", { profileId }),
  openInExplorer: (path: string) => invoke<void>("open_in_explorer", { path }),
  pathExists: (path: string) => invoke<boolean>("path_exists", { path }),
  modelFileStatus: (profileId: string) => invoke<T.ModelFileStatus>("model_file_status", { profileId }),
  hfList: (repo: string) => invoke<T.HfFile[]>("hf_list", { repo }),
  downloadModel: (profileId: string, file: string, size: number, sha256: string | null) =>
    invoke<T.DownloadOutcome>("download_model", { profileId, file, size, sha256 }),
  verifyModel: (profileId: string) => invoke<{ sha256: string; expected: string; matches: boolean }>("verify_model", { profileId }),
  llamaRelease: () => invoke<T.LlamaRelease>("llama_release"),
  downloadLlama: (release: T.LlamaRelease) => invoke<string>("download_llama", { release }),
};

export function errText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}

export function uid(): string {
  return crypto.randomUUID();
}

export function fmtBytes(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} MB`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(0)} KB`;
  return `${n} B`;
}

export function fmtCtx(n: number): string {
  return n >= 1024 ? `${Math.round(n / 1024)}k` : `${n}`;
}

export function fmtTokens(n: number): string {
  return n >= 1000 ? `${(n / 1000).toFixed(1)}k` : `${n}`;
}

export function fmtSecs(ms: number): string {
  return ms >= 60000 ? `${Math.floor(ms / 60000)}m ${Math.round((ms % 60000) / 1000)}s` : `${(ms / 1000).toFixed(1)}s`;
}
