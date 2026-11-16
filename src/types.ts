// Mirrors the Rust DTOs (serde camelCase).

export type ServerState = "unloaded" | "loading" | "ready" | "error";
export type PaneStatus = "idle" | "running" | "done" | "error";
export type SplitMode = "single" | "perFile";
export type ProfileKind = "managed" | "external";
export type ClaudeBackend = "cli" | "api";
export type PlannerBackend = "local" | "claude";
export type VerifyKind = "setup" | "build" | "test" | "run";
export type StepKind = "stage" | "implement" | "verify" | "fix" | "review" | "followUp";
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
  /** Sampling for the planner role (thinking on); null → `sampling`. */
  thinkingSampling: Sampling | null;
  reasoning: ReasoningControl;
  external: ExternalEndpoint;
}

export interface Settings {
  modelsDir: string;
  llamaServerPath: string;
  serverPort: number;
  activeProfile: string;
  plannerBackend: PlannerBackend;
  plannerProfile: string;
  plannerEffort: string;
  plannerTools: boolean;
  coderProfile: string;
  verifyEnabled: boolean;
  maxFixRounds: number;
  loopReview: boolean;
  followUpRounds: number;
  buildTimeoutSecs: number;
  runTimeoutSecs: number;
  autopilot: boolean;
  /** Autopilot secure mode: allowlisted commands only, run sandboxed. */
  autopilotSecure: boolean;
  /** Sandbox the verification of every run, also reviewed ones. */
  sandboxAllRuns: boolean;
  claudeBackend: ClaudeBackend;
  claudeModel: string;
  claudeEffort: string;
  claudeCliExplore: boolean;
  reviewEnabled: boolean;
  autoFallback: boolean;
  fallbackProfile: string;
  outlineContext: boolean;
  prewarmPrompt: boolean;
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
  /** Class-diagram-level design, shared by every per-file call. */
  design?: string;
  files: SpecFile[];
  changes: [string, string][];
  edgeCases: string;
  acceptance: string;
  mentionedPaths: string[];
  verification?: VerifyCommand[];
  verificationDeclared?: boolean;
  warnings: string[];
}

export interface VerifyCommand {
  kind: VerifyKind;
  command: string;
  timeoutSecs: number | null;
  expect: string | null;
}

export interface VerifyPreview {
  enabled: boolean;
  commands: VerifyCommand[];
  /** "spec", "detected" or "off". */
  source: string;
  detectedFrom: string[];
  warnings: string[];
  /** Commands autopilot secure mode would not run unattended. */
  blocked: Blocked[];
}

export interface Blocked {
  command: string;
  reason: string;
}

export interface AutopilotCheck {
  secure: boolean;
  sandboxed: boolean;
  commands: VerifyCommand[];
  blocked: Blocked[];
}

export interface CommandResult {
  kind: VerifyKind | null;
  command: string;
  exitCode: number | null;
  timedOut: boolean;
  cancelled: boolean;
  passed: boolean;
  durationMs: number;
  output: string;
  expectMissing: boolean;
  spawnError: string | null;
}

export interface WorkflowStep {
  kind: StepKind;
  label: string;
  ok: boolean;
  durationMs: number;
  detail: string;
  commands: CommandResult[];
  review: Review | null;
}

export interface WorkflowReport {
  stagingDir: string;
  planner: string;
  coder: string;
  commands: VerifyCommand[];
  commandsSource: string;
  steps: WorkflowStep[];
  verified: boolean | null;
  review: Review | null;
  outcome: string;
  /** The commands ran in the sandbox (Low integrity). */
  sandboxed: boolean;
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
  /** Token counts come from the loaded model's tokenizer (false: length estimate). */
  exact?: boolean;
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
  /** What the run will verify (missing in older builds). */
  verify?: VerifyPreview | null;
}

export interface ClaudeResult {
  text: string;
  backend: string;
  model: string;
  durationMs: number;
  costUsd: number | null;
  stopReason: string | null;
  cancelled: boolean;
  /** Prompt tokens read from Claude's prompt cache (all turns of the expansion). */
  cacheReadTokens?: number | null;
  cacheWriteTokens?: number | null;
  /** Local planner: generation speed and the tool calls it made. */
  tokPerSec?: number | null;
  toolCalls?: string[];
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
  /** Prompt tokens the server reused from its prompt cache (missing in older history entries). */
  cachedTokens?: number | null;
  completionTokens: number | null;
  promptPerSec?: number | null;
  /** Speculative decoding: tokens drafted / accepted (llama-server timings). */
  draftTokens?: number | null;
  draftAccepted?: number | null;
  durationMs: number;
  finishReason: string | null;
  error: string | null;
}

export interface CallReport {
  index: number;
  target: string | null;
  attempts: AttemptReport[];
  ok: boolean;
  /** "Fix 1", "Follow-up 1"; null for the first implementation pass. */
  label?: string | null;
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
  /** Staging, verification, fix rounds and review (absent for plain runs). */
  workflow?: WorkflowReport | null;
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
  kind:
    | "start"
    | "callStart"
    | "delta"
    | "reasoningDelta"
    | "stats"
    | "callEnd"
    | "retry"
    | "done"
    | "primed"
    | "phase"
    | "stepEnd"
    | "cmdStart"
    | "cmdOutput"
    | "cmdEnd"
    | "reviewDelta"
    | "reviewReasoning"
    | "reviewTool"
    | "reviewSegment"
    | "reviewStats"
    | "reviewStatus";
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
  /** `primed`: what was evaluated into the server's prompt cache. */
  label?: string;
  cached?: number | null;
  processed?: number | null;
  ms?: number;
  /** Loop events: the step, a command, its outcome. */
  step?: StepKind | VerifyKind;
  command?: string;
  passed?: boolean;
  exitCode?: number | null;
  timedOut?: boolean;
  durationMs?: number;
  summary?: string;
  detail?: string;
  timeoutSecs?: number;
  sandboxed?: boolean;
}

export interface PrimeOutcome {
  label: string;
  tokens: number;
  cached: number | null;
  processed: number | null;
  ms: number;
}

export interface PrewarmResult {
  status: "primed" | "warm" | "skipped" | "unsupported" | "cancelled" | "error" | "running";
  message: string;
  outcomes: PrimeOutcome[];
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
