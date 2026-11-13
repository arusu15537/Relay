// Mirrors the Rust DTOs (serde camelCase).

export type ServerState = "unloaded" | "loading" | "ready" | "error";
export type PaneStatus = "idle" | "running" | "done" | "error";
export type SplitMode = "single" | "perFile";
export type ProfileKind = "managed" | "external";
export type ClaudeBackend = "cli" | "api";
export type ReasoningMode = "disable_thinking" | "effort" | "none";
export type FileAction = "modify" | "create" | "delete";

export interface Sampling {
  temperature: number;
  topP: number;
  topK: number;
  minP: number;
  presencePenalty: number;
  repeatPenalty: number;
}

export interface ReasoningControl {
  mode: ReasoningMode;
  effort: string;
  stripThinkTags: boolean;
  stripHarmony: boolean;
  reasoningReserve: number;
}

export interface ExternalEndpoint {
  baseUrl: string;
  model: string;
  useApiKey: boolean;
}

export interface Profile {
  id: string;
  builtin: boolean;
  name: string;
  role: string;
  kind: ProfileKind;
  ggufPath: string;
  hfRepo: string;
  hfFile: string;
  hfSize: number;
  hfSha256: string;
  launchArgs: string;
  contextLimit: number;
  maxOutputTokens: number;
  sampling: Sampling;
  reasoning: ReasoningControl;
  external: ExternalEndpoint;
}

export interface Settings {
  modelsDir: string;
  llamaServerPath: string;
  serverPort: number;
  activeProfile: string;
  claudeBackend: ClaudeBackend;
  claudeModel: string;
  claudeEffort: string;
  claudeCliExplore: boolean;
  reviewEnabled: boolean;
  autoFallback: boolean;
  fallbackProfile: string;
  outlineContext: boolean;
  lastProject: string;
  loadTimeoutSecs: number;
  loadStats: Record<string, number>;
}

export interface ServerStatus {
  state: ServerState;
  profileId: string | null;
  profileName: string | null;
  external: boolean;
  message: string;
  progress: number | null;
  baseUrl: string | null;
  hint: string | null;
  loadSecs: number | null;
  pid: number | null;
}

export interface AppStateDto {
  settings: Settings;
  profiles: Profile[];
  server: ServerStatus;
  cliPath: string | null;
  llamaServer: string | null;
  appDir: string;
  templatesDir: string;
  startupNotes: string[];
  hasApiKey: boolean;
  version: string;
}

export interface FileEntry {
  path: string;
  size: number;
}

export interface FileList {
  root: string;
  files: FileEntry[];
  truncated: boolean;
}

export interface GitStatus {
  isRepo: boolean;
  branch: string;
  dirty: boolean;
  changes: string[];
  error: string | null;
}

export interface SpecFile {
  path: string;
  action: FileAction;
  note: string;
}

export interface ParsedSpec {
  summary: string;
  files: SpecFile[];
  changes: [string, string][];
  edgeCases: string;
  acceptance: string;
  mentionedPaths: string[];
  warnings: string[];
}

export interface CallBudget {
  target: string | null;
  contextFiles: string[];
  inputTokens: number;
  totalTokens: number;
  fits: boolean;
}

export interface BudgetPlan {
  contextLimit: number;
  usable: number;
  reservedOutput: number;
  single: CallBudget;
  perFile: CallBudget[];
  recommended: SplitMode;
  warnings: string[];
}

export interface PreparedFile {
  path: string;
  exists: boolean;
  target: boolean;
  action: FileAction | null;
  /** Tokens sent to the model (the outline's size when `outline`). */
  tokens: number;
  fullTokens: number;
  /** Set for context files where an outline is available and worthwhile. */
  outlineTokens: number | null;
  outline: boolean;
  error: string | null;
}

export interface Prepared {
  spec: ParsedSpec;
  files: PreparedFile[];
  budget: BudgetPlan;
  warnings: string[];
}

export interface ClaudeResult {
  text: string;
  backend: string;
  model: string;
  durationMs: number;
  costUsd: number | null;
  stopReason: string | null;
  cancelled: boolean;
}

export interface ExpandResult {
  jobId: string;
  spec: string;
  parsed: ParsedSpec;
  claude: ClaudeResult;
  warnings: string[];
}

export interface BlockOutcome {
  index: number;
  path: string;
  ok: boolean;
  kind: string | null;
  message: string | null;
  closest: [number, string] | null;
  searchPreview: string;
}

export interface ParseIssue {
  line: number;
  message: string;
}

export interface AttemptReport {
  attempt: number;
  rawOutput: string;
  reasoning: string;
  blocks: number;
  outcomes: BlockOutcome[];
  parseIssues: ParseIssue[];
  missingTargets: string[];
  ok: boolean;
  tokPerSec: number | null;
  promptTokens: number | null;
  completionTokens: number | null;
  durationMs: number;
  finishReason: string | null;
  error: string | null;
}

export interface CallReport {
  index: number;
  target: string | null;
  attempts: AttemptReport[];
  ok: boolean;
}

export interface ProposedFile {
  path: string;
  original: string | null;
  proposed: string;
}

export interface ImplementResult {
  runId: string;
  profileId: string;
  profileName: string;
  mode: SplitMode;
  budget: BudgetPlan;
  files: ProposedFile[];
  calls: CallReport[];
  validationOk: boolean;
  cancelled: boolean;
  error: string | null;
  warnings: string[];
  durationMs: number;
  avgTokPerSec: number | null;
}

export interface ImplementResponse {
  result: ImplementResult;
  fallbackSuggested: boolean;
  fallbackProfile: string | null;
}

export interface AcceptOutcome {
  ok: boolean;
  conflict: "changedOnDisk" | "nowExists" | null;
  message: string;
}

export interface Review {
  verdict: string;
  passed: boolean;
  report: string;
  followUpSpec: string | null;
}

export interface ProfileTest {
  ok: boolean;
  message: string;
  output: string;
  reasoningChars: number;
  tokPerSec: number | null;
  loadSecs: number | null;
  formatOk: boolean;
}

export interface RunSummary {
  id: string;
  profileName: string;
  status: string;
  validationOk: boolean;
  avgTokPerSec: number | null;
  durationMs: number;
  accepted: number;
  rejected: number;
  files: number;
}

export interface JobSummary {
  id: string;
  createdAt: string;
  projectRoot: string;
  instruction: string;
  runs: RunSummary[];
}

export interface RunFileRecord {
  path: string;
  original: string | null;
  proposed: string;
  decision: string;
  decidedAt: string | null;
  finalContent: string | null;
}

export interface JobRecord {
  id: string;
  createdAt: string;
  projectRoot: string;
  instruction: string;
  spec: string;
  specEdited: boolean;
  claudeBackend: string;
  claudeModel: string;
  expandMs: number | null;
  expandCost: number | null;
  contextFiles: string[];
}

export interface RunRecord {
  id: string;
  jobId: string;
  createdAt: string;
  profileId: string;
  profileName: string;
  modelFile: string;
  mode: string;
  spec: string;
  status: string;
  validationOk: boolean;
  durationMs: number;
  avgTokPerSec: number | null;
  error: string | null;
  result: ImplementResult | null;
  review: Review | null;
  files: RunFileRecord[];
}

export interface JobDetail {
  job: JobRecord;
  runs: RunRecord[];
}

export interface TemplateInfo {
  name: string;
  path: string;
  text: string;
  isDefault: boolean;
}

export interface HfFile {
  path: string;
  size: number;
  sha256: string | null;
}

export interface DownloadOutcome {
  path: string;
  bytes: number;
  sha256: string;
  verified: boolean;
  resumedFrom: number;
  cancelled: boolean;
}

export interface ModelFileStatus {
  path: string;
  exists: boolean;
  size: number;
  expectedSize: number;
  partial: number;
}

export interface LlamaRelease {
  tag: string;
  serverZip: [string, string, number];
  cudartZip: [string, string, number] | null;
}

export interface RunEvent {
  runId: string;
  kind: "start" | "callStart" | "delta" | "reasoningDelta" | "stats" | "callEnd" | "retry" | "done";
  call?: number;
  attempt?: number;
  text?: string;
  target?: string | null;
  tokPerSec?: number;
  tokens?: number;
  ok?: boolean;
  error?: string;
  message?: string;
  calls?: number;
  mode?: SplitMode;
}

export interface ClaudeEvent {
  jobId: string;
  kind: "delta" | "thinking" | "tool" | "status" | "segment" | "done" | "error" | "cancelled";
  text: string;
}

export interface DownloadProgress {
  id: string;
  phase: string;
  done: number;
  total: number;
  bytesPerSec: number;
}
