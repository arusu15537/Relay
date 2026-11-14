import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errText, uid } from "./api";
import { ask, get, profileName, RunView, set, toast, updateRun } from "./store";
import type * as T from "./types";

// ---------------------------------------------------------------------------------------------
// Boot + events
// ---------------------------------------------------------------------------------------------

export async function boot() {
  const st = await api.getAppState();
  set({
    boot: st,
    settings: st.settings,
    profiles: st.profiles,
    server: st.server,
    implementProfile: st.settings.activeProfile,
    ready: true,
  });
  for (const n of st.startupNotes) toast(n);
  await listen<T.ServerStatus>("server-status", (e) => set({ server: e.payload }));
  await listen<string>("server-log", (e) =>
    set((s) => ({ log: s.log.length > 3000 ? [...s.log.slice(-2500), e.payload] : [...s.log, e.payload] })),
  );
  await listen<T.RunEvent>("run-event", (e) => onRunEvent(e.payload));
  await listen<T.ClaudeEvent>("claude-event", (e) => onClaudeEvent(e.payload));
  await listen<T.DownloadProgress>("download-progress", (e) =>
    set((s) => ({ downloads: { ...s.downloads, [e.payload.id]: e.payload } })),
  );
  set({ log: await api.serverLog() });
  pollGpu();
  if (st.settings.lastProject && (await api.pathExists(st.settings.lastProject))) {
    await openProject(st.settings.lastProject);
  }
}

function pollGpu() {
  const tick = async () => {
    if (document.visibilityState === "visible") {
      try {
        set({ gpu: await api.gpuMemory() });
      } catch {
        /* nvidia-smi missing */
      }
    }
  };
  tick();
  setInterval(tick, 3000);
}

function onRunEvent(ev: T.RunEvent) {
  const run = get().runs.find((r) => r.runId === ev.runId);
  if (!run) return;
  switch (ev.kind) {
    case "start":
      updateRun(ev.runId, () => ({ notes: [`Mode: ${ev.mode === "perFile" ? "one call per file" : "single call"}, ${ev.calls} call(s)`] }));
      break;
    case "callStart":
      updateRun(ev.runId, (r) => ({
        stream: "",
        reasoning: "",
        callLabel: `${ev.target ?? "All files"} — call ${(ev.call ?? 0) + 1}, attempt ${ev.attempt}`,
        notes: r.notes,
      }));
      break;
    case "delta":
      updateRun(ev.runId, (r) => ({ stream: r.stream + (ev.text ?? "") }));
      break;
    case "reasoningDelta":
      updateRun(ev.runId, (r) => ({ reasoning: r.reasoning + (ev.text ?? "") }));
      break;
    case "stats":
      updateRun(ev.runId, () => ({ liveTps: ev.tokPerSec ?? null, tokens: ev.tokens ?? 0 }));
      break;
    case "callEnd": {
      const note = ev.error
        ? `Call ${(ev.call ?? 0) + 1}: ${ev.error}`
        : `Call ${(ev.call ?? 0) + 1}, attempt ${ev.attempt}: ${ev.ok ? "valid" : "validation failed"}${ev.tokPerSec ? ` · ${ev.tokPerSec.toFixed(1)} tok/s` : ""}`;
      updateRun(ev.runId, (r) => ({ notes: [...r.notes, note], liveTps: ev.tokPerSec ?? r.liveTps }));
      break;
    }
    case "retry":
      updateRun(ev.runId, (r) => ({ notes: [...r.notes, `Automatic retry for call ${(ev.call ?? 0) + 1} (errors sent back to the model)`] }));
      break;
  }
}

function onClaudeEvent(ev: T.ClaudeEvent) {
  const s = get();
  if (ev.jobId === s.jobId && s.expandStatus === "running") {
    switch (ev.kind) {
      case "delta":
        set((st) => ({ claudeStream: st.claudeStream + ev.text }));
        break;
      case "segment":
        set({ claudeStream: "" });
        break;
      case "thinking":
        set((st) => ({ claudeThinking: (st.claudeThinking + ev.text).slice(-4000) }));
        break;
      case "tool":
        set((st) => ({ claudeActivity: [...st.claudeActivity, ev.text] }));
        break;
      case "status":
        set({ expandInfo: ev.text });
        break;
    }
    return;
  }
  const run = s.runs.find((r) => r.reviewJobId === ev.jobId);
  if (run && ev.kind === "delta") updateRun(run.runId, (r) => ({ reviewStream: r.reviewStream + ev.text }));
}

// ---------------------------------------------------------------------------------------------
// Settings / profiles / server
// ---------------------------------------------------------------------------------------------

export async function saveSettings(patch: Partial<T.Settings>) {
  const cur = get().settings;
  if (!cur) return;
  const next = { ...cur, ...patch };
  set({ settings: next });
  try {
    await api.saveSettings(next);
  } catch (e) {
    toast(errText(e), "error");
  }
}

export async function saveProfiles(list: T.Profile[]): Promise<boolean> {
  try {
    const saved = await api.saveProfiles(list);
    set({ profiles: saved });
    return true;
  } catch (e) {
    toast(errText(e), "error");
    return false;
  }
}

export async function switchProfile(id: string) {
  if (isBusy()) {
    toast("Wait for the current run to finish (or cancel it) before switching models.", "error");
    return;
  }
  set((s) => ({ implementProfile: id, settings: s.settings ? { ...s.settings, activeProfile: id } : s.settings }));
  try {
    const st = await api.switchProfile(id);
    if (st.state === "error") toast(`${st.message}${st.hint ? ` — ${st.hint}` : ""}`, "error");
  } catch (e) {
    toast(errText(e), "error");
  }
  schedulePrepare();
}

export async function stopServer() {
  try {
    await api.stopServer();
  } catch (e) {
    toast(errText(e), "error");
  }
}

export function isBusy(): boolean {
  const s = get();
  return s.expandStatus === "running" || s.runs.some((r) => r.status === "running");
}

// ---------------------------------------------------------------------------------------------
// Project
// ---------------------------------------------------------------------------------------------

export async function pickProject() {
  const dir = await open({ directory: true, multiple: false, title: "Choose the target project folder" });
  if (typeof dir === "string") await openProject(dir);
}

export async function openProject(root: string) {
  try {
    const list = await api.listFiles(root);
    set({ root, files: list.files, filesTruncated: list.truncated, checked: [], fullContext: [] });
    saveSettings({ lastProject: root });
    refreshGit();
    schedulePrepare();
  } catch (e) {
    toast(errText(e), "error");
  }
}

export async function refreshFiles() {
  const root = get().root;
  if (!root) return;
  const list = await api.listFiles(root);
  set((s) => ({ files: list.files, filesTruncated: list.truncated, checked: s.checked.filter((c) => list.files.some((f) => f.path === c)) }));
}

export async function refreshGit() {
  const root = get().root;
  if (root) set({ git: await api.gitStatus(root) });
}

export function toggleChecked(paths: string[], on: boolean) {
  set((s) => {
    const cur = new Set(s.checked);
    for (const p of paths) {
      if (on) cur.add(p);
      else cur.delete(p);
    }
    return { checked: [...cur] };
  });
  schedulePrepare();
}

/** Send a read-only context file in full instead of as an outline, or back. */
export function toggleFullContext(path: string) {
  set((s) => ({ fullContext: s.fullContext.includes(path) ? s.fullContext.filter((p) => p !== path) : [...s.fullContext, path] }));
  schedulePrepare(0);
}

// ---------------------------------------------------------------------------------------------
// Expand (Claude)
// ---------------------------------------------------------------------------------------------

export async function expand() {
  const s = get();
  if (!s.root) return toast("Choose a project folder first", "error");
  if (!s.instruction.trim()) return toast("Type an instruction first", "error");
  const jobId = uid();
  set({
    jobId,
    expandStatus: "running",
    specStatus: "running",
    claudeStream: "",
    claudeThinking: "",
    claudeActivity: [],
    expandInfo: "Starting Claude…",
    expandError: null,
    spec: "",
    prepared: null,
    runs: [],
    activeRun: null,
    compareWith: null,
  });
  try {
    const res = await api.expand({ jobId, instruction: s.instruction, projectRoot: s.root, contextFiles: s.checked, profileId: s.implementProfile });
    if (res.claude.cancelled) {
      set({ expandStatus: "idle", specStatus: get().claudeStream ? "done" : "idle", spec: get().claudeStream, expandInfo: "Cancelled" });
      return;
    }
    const cost = res.claude.costUsd != null ? ` · $${res.claude.costUsd.toFixed(3)}` : "";
    const cached = res.claude.cacheReadTokens ? ` · ${(res.claude.cacheReadTokens / 1000).toFixed(1)}k tokens from cache` : "";
    set({
      spec: res.spec,
      expandStatus: "done",
      specStatus: res.parsed.files.length ? "done" : "error",
      expandInfo: `Spec written by Claude (${res.claude.backend.toUpperCase()}) in ${(res.claude.durationMs / 1000).toFixed(0)}s${cost}${cached}`,
    });
    for (const w of res.warnings) toast(w);
    schedulePrepare(0);
  } catch (e) {
    set({ expandStatus: "error", specStatus: "idle", expandError: errText(e), expandInfo: "" });
  }
}

export async function cancelExpand() {
  await api.cancelJob(get().jobId);
}

export function setSpec(spec: string) {
  set({ spec, specStatus: spec.trim() ? "done" : "idle" });
  schedulePrepare();
}

let prepTimer: number | undefined;
export function schedulePrepare(delay = 350) {
  window.clearTimeout(prepTimer);
  prepTimer = window.setTimeout(async () => {
    const s = get();
    if (!s.root || !s.spec.trim() || !s.implementProfile) {
      set({ prepared: null });
      return;
    }
    try {
      const prepared = await api.prepareImplement(s.root, s.spec, s.checked, s.fullContext, s.implementProfile);
      set({ prepared, specStatus: get().expandStatus === "running" ? "running" : prepared.spec.files.length ? "done" : "error" });
    } catch (e) {
      set({ prepared: null });
      toast(errText(e), "error");
    }
  }, delay);
}

// ---------------------------------------------------------------------------------------------
// Implement (local model)
// ---------------------------------------------------------------------------------------------

function newRun(runId: string, profileId: string, spec: string): RunView {
  return {
    runId,
    profileId,
    profileName: profileName(profileId),
    status: "running",
    stream: "",
    reasoning: "",
    callLabel: "Waiting for the model…",
    liveTps: null,
    tokens: 0,
    notes: [],
    response: null,
    error: null,
    decisions: {},
    edited: {},
    review: null,
    reviewStatus: "idle",
    reviewStream: "",
    reviewJobId: uid(),
    startedAt: Date.now(),
    spec,
    fromHistory: false,
  };
}

export async function implement(profileId?: string, specOverride?: string) {
  const s = get();
  const pid = profileId ?? s.implementProfile;
  const spec = specOverride ?? s.spec;
  if (!s.root) return toast("Choose a project folder first", "error");
  if (!spec.trim()) return toast("The spec is empty", "error");
  if (s.runs.some((r) => r.status === "running")) return toast("A run is already in progress", "error");
  const runId = uid();
  const run = newRun(runId, pid, spec);
  const st = get().server;
  if (st?.profileId !== pid || st?.state !== "ready") run.callLabel = `Loading ${run.profileName}…`;
  set((st2) => ({ runs: [...st2.runs, run], activeRun: runId, compareWith: null }));
  try {
    const resp = await api.implement({
      jobId: s.jobId,
      runId,
      projectRoot: s.root,
      instruction: s.instruction,
      spec,
      contextFiles: s.checked,
      mode: s.mode === "auto" ? null : s.mode,
      profileId: pid,
      fullContext: s.fullContext,
    });
    const r = resp.result;
    const decisions: Record<string, "pending"> = {};
    for (const f of r.files) decisions[f.path] = "pending";
    updateRun(runId, () => ({
      status: r.cancelled ? "idle" : r.validationOk ? "done" : "error",
      response: resp,
      decisions,
      error: r.cancelled ? "Cancelled" : r.error,
      callLabel: r.cancelled ? "Cancelled" : r.validationOk ? "Done" : "Validation failed",
    }));
    if (resp.fallbackSuggested && resp.fallbackProfile) {
      const fb = resp.fallbackProfile;
      const choice = await ask(
        "Edit-format validation failed twice",
        `${profileName(pid)}'s output still had SEARCH blocks that don't match the files after the automatic retry. Retry the same spec with ${profileName(fb)}?`,
        [
          { id: "retry", label: `Retry with ${profileName(fb)}`, kind: "primary" },
          { id: "cancel", label: "Not now" },
        ],
      );
      if (choice === "retry") await implement(fb, spec);
    }
  } catch (e) {
    updateRun(runId, () => ({ status: "error", error: errText(e), callLabel: "Error" }));
  }
}

export async function cancelRun(runId: string) {
  await api.cancelJob(runId);
}

// ---------------------------------------------------------------------------------------------
// Accept / reject
// ---------------------------------------------------------------------------------------------

const gitChecked = new Set<string>();

/** Before the first write of a run: warn about a dirty git tree and offer a checkpoint commit. */
async function gitGate(runId: string): Promise<boolean> {
  if (gitChecked.has(runId)) return true;
  const root = get().root;
  const g = await api.gitStatus(root);
  set({ git: g });
  if (g.isRepo && g.dirty) {
    const choice = await ask(
      "Uncommitted changes in the project",
      `The working tree on '${g.branch}' has ${g.changes.length} uncommitted change(s). Accepted files will be written on top of them.`,
      [
        { id: "checkpoint", label: "Create checkpoint commit, then apply", kind: "primary" },
        { id: "apply", label: "Apply anyway" },
        { id: "cancel", label: "Cancel" },
      ],
      g.changes.slice(0, 12),
    );
    if (choice === "cancel") return false;
    if (choice === "checkpoint") {
      try {
        const hash = await api.gitCheckpoint(root, `Relay checkpoint before: ${get().instruction.slice(0, 60) || "applying changes"}`);
        toast(`Checkpoint commit ${hash} created`, "ok");
      } catch (e) {
        toast(`Checkpoint failed: ${errText(e)}`, "error");
        return false;
      }
    }
  }
  gitChecked.add(runId);
  return true;
}

export async function acceptFile(runId: string, path: string): Promise<boolean> {
  if (!(await gitGate(runId))) return false;
  const run = get().runs.find((r) => r.runId === runId);
  const content = run?.edited[path] ?? null;
  try {
    let out = await api.acceptFile(runId, path, content, false);
    if (!out.ok && out.conflict) {
      const c = await ask("File changed on disk", out.message, [
        { id: "overwrite", label: "Overwrite", kind: "danger" },
        { id: "cancel", label: "Cancel" },
      ]);
      if (c !== "overwrite") return false;
      out = await api.acceptFile(runId, path, content, true);
    }
    if (out.ok) {
      updateRun(runId, (r) => ({ decisions: { ...r.decisions, [path]: "accepted" } }));
      afterDecision(runId);
      return true;
    }
    toast(out.message, "error");
  } catch (e) {
    toast(errText(e), "error");
  }
  return false;
}

export async function rejectFile(runId: string, path: string) {
  try {
    await api.rejectFile(runId, path);
    updateRun(runId, (r) => ({ decisions: { ...r.decisions, [path]: "rejected" } }));
    afterDecision(runId);
  } catch (e) {
    toast(errText(e), "error");
  }
}

export async function acceptAll(runId: string) {
  const run = get().runs.find((r) => r.runId === runId);
  if (!run) return;
  for (const [path, d] of Object.entries(run.decisions)) {
    if (d === "pending" && !(await acceptFile(runId, path))) break;
  }
}

function afterDecision(runId: string) {
  refreshGit();
  refreshFiles();
  const run = get().runs.find((r) => r.runId === runId);
  if (!run) return;
  const values = Object.values(run.decisions);
  const allDecided = values.length > 0 && values.every((d) => d !== "pending");
  const anyAccepted = values.some((d) => d === "accepted");
  if (allDecided && anyAccepted && get().settings?.reviewEnabled && run.reviewStatus === "idle") {
    review(runId);
  }
}

export function setEdited(runId: string, path: string, content: string) {
  updateRun(runId, (r) => ({ edited: { ...r.edited, [path]: content } }));
}

// ---------------------------------------------------------------------------------------------
// Review (Claude)
// ---------------------------------------------------------------------------------------------

export async function review(runId: string) {
  const run = get().runs.find((r) => r.runId === runId);
  if (!run) return;
  const reviewJobId = uid();
  updateRun(runId, () => ({ reviewStatus: "running", reviewStream: "", reviewJobId }));
  try {
    const rv = await api.reviewRun(runId, reviewJobId);
    updateRun(runId, () => ({ review: rv, reviewStatus: rv.passed ? "done" : "error" }));
  } catch (e) {
    updateRun(runId, () => ({ reviewStatus: "idle" }));
    toast(`Review: ${errText(e)}`, "error");
  }
}

export function applyFollowUp(runId: string) {
  const run = get().runs.find((r) => r.runId === runId);
  const follow = run?.review?.followUpSpec;
  if (!follow) return;
  set((s) => ({
    jobId: uid(),
    instruction: `Follow-up: ${s.instruction}`.slice(0, 500),
    spec: follow,
    specStatus: "done",
    runs: [],
    activeRun: null,
    compareWith: null,
    expandStatus: "done",
    expandInfo: "Follow-up spec from Claude's review",
  }));
  schedulePrepare(0);
}

// ---------------------------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------------------------

export async function loadJob(jobId: string) {
  const d = await api.historyGet(jobId);
  if (!d) return toast("Job not found", "error");
  if (d.job.projectRoot && d.job.projectRoot !== get().root) await openProject(d.job.projectRoot);
  const runs: RunView[] = d.runs.map((r) => {
    const decisions: RunView["decisions"] = {};
    for (const f of r.files) decisions[f.path] = (f.decision as "pending" | "accepted" | "rejected") ?? "pending";
    const response: T.ImplementResponse | null = r.result ? { result: r.result, fallbackSuggested: false, fallbackProfile: null } : null;
    return {
      ...newRun(r.id, r.profileId, r.spec),
      profileName: r.profileName,
      status: r.status === "ok" ? "done" : r.status === "cancelled" ? "idle" : "error",
      callLabel: `From history · ${new Date(r.createdAt).toLocaleString()}`,
      response,
      decisions,
      error: r.error,
      review: r.review,
      reviewStatus: r.review ? (r.review.passed ? "done" : "error") : "idle",
      fromHistory: true,
    };
  });
  set({
    jobId: d.job.id,
    instruction: d.job.instruction,
    spec: d.job.spec,
    specStatus: d.job.spec ? "done" : "idle",
    checked: d.job.contextFiles,
    fullContext: [],
    expandStatus: d.job.spec ? "done" : "idle",
    expandInfo: d.job.claudeBackend ? `Spec by Claude (${d.job.claudeBackend})${d.job.specEdited ? ", edited" : ""}` : "",
    runs,
    activeRun: runs.length ? runs[runs.length - 1].runId : null,
    compareWith: null,
    historyOpen: false,
  });
  schedulePrepare(0);
}

export function newJob() {
  if (isBusy()) return toast("Finish or cancel the current step first", "error");
  set({
    jobId: uid(),
    instruction: "",
    spec: "",
    specStatus: "idle",
    prepared: null,
    expandStatus: "idle",
    claudeStream: "",
    claudeActivity: [],
    expandInfo: "",
    expandError: null,
    runs: [],
    activeRun: null,
    compareWith: null,
  });
}
