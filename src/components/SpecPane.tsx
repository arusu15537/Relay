import Editor from "@monaco-editor/react";
import { cancelRun, implement, schedulePrepare, schedulePrewarm, setSpec, toggleFullContext } from "../actions";
import { fmtCtx, fmtTokens } from "../api";
import { set, useStore } from "../store";
import type { Prepared, VerifyPreview } from "../types";
import { StatusDot, useDarkMode } from "./common";

const SKELETON = `## Summary
What should change and why, in 2–5 sentences.

## Files
- \`path/to/file.ext\` (modify) — purpose
- \`path/to/new_file.ext\` (create) — purpose

## Changes

### \`path/to/file.ext\`
1. Exactly where and what to change: signatures (\`name(arg: type) -> type\`) and the behaviour in words.
   Describe the logic; the local model writes the code.

### \`path/to/new_file.ext\`
Everything the file must contain (imports, constants, functions with signatures) and what each one does.

## Edge cases
- …

## Acceptance criteria
- …

## Verification
- build: \`command that builds the project\`
- test: \`command that runs the tests\`
`;

/** Show tool activity paths relative to the project root. */
function relToRoot(text: string, root: string): string {
  if (!root) return text;
  const norm = (x: string) => x.split("\\").join("/");
  const r = norm(root).replace(/\/$/, "") + "/";
  const t = norm(text);
  const i = t.toLowerCase().indexOf(r.toLowerCase());
  return i >= 0 ? t.slice(0, i) + t.slice(i + r.length) : text;
}

function Verification({ v }: { v: VerifyPreview }) {
  const autopilot = useStore((s) => s.settings?.autopilot ?? false);
  const secure = useStore((s) => s.settings?.autopilotSecure ?? true);
  const sandboxAll = useStore((s) => s.settings?.sandboxAllRuns ?? false);
  const blocked = new Map((v.blocked ?? []).map((b) => [b.command, b.reason]));
  const source = v.source === "spec" ? "from the spec's Verification section" : v.source === "detected" ? `detected from ${v.detectedFrom.join(", ") || "the project"}` : "";
  return (
    <div className="col">
      <span className="section-label">Verification</span>
      {!v.enabled ? (
        <span className="faint" style={{ fontSize: 12 }}>
          Off (Settings → Roles): the change is not built or tested before you see it.
        </span>
      ) : v.commands.length === 0 ? (
        <span className="faint" style={{ fontSize: 12 }}>
          Nothing to run{v.source === "spec" ? " (the spec says nothing can be checked)" : " — no build or test setup detected; add a ## Verification section to the spec"}.
        </span>
      ) : (
        <>
          <div className="chips">
            {v.commands.map((c, i) => (
              <span
                key={i}
                className={`chip ctx mono${autopilot && secure && blocked.has(c.command) ? " warn" : ""}`}
                title={`${c.kind}${c.timeoutSecs ? ` · timeout ${c.timeoutSecs}s` : ""}${c.expect ? ` · output must contain "${c.expect}"` : ""}${blocked.has(c.command) ? ` · secure mode: ${blocked.get(c.command)}` : ""}`}
              >
                <b>{c.kind}</b> {c.command}
              </span>
            ))}
          </div>
          <span className="faint" style={{ fontSize: 12 }}>
            Run in a staging copy of the project, {source}
            {sandboxAll ? ", sandboxed" : ""}. Failures go back to the coder.
          </span>
          {autopilot && secure && blocked.size > 0 && (
            <div className="note warn">
              Secure mode won't run {[...blocked.keys()].map((c) => `\`${c}\``).join(", ")} unattended, so autopilot stops at this spec. Implement runs it after your review.
            </div>
          )}
        </>
      )}
      {v.warnings.map((w, i) => (
        <div key={i} className="note warn">
          {w}
        </div>
      ))}
    </div>
  );
}

function Budget({ prepared }: { prepared: Prepared }) {
  const mode = useStore((s) => s.mode);
  const b = prepared.budget;
  const effective = mode === "auto" ? b.recommended : mode;
  const total = b.single.totalTokens;
  const inPct = Math.min(100, (b.single.inputTokens / b.contextLimit) * 100);
  const outPct = Math.min(100 - inPct, (b.reservedOutput / b.contextLimit) * 100);
  const over = !b.single.fits;
  const prewarm = useStore((s) => s.prewarm);
  return (
    <div className="budget">
      <div className="row">
        <span className="section-label">Context budget</span>
        <span className="muted" style={{ fontSize: 12 }}>
          {b.exact ? "" : "≈"}
          {fmtTokens(b.single.inputTokens)} in + {fmtTokens(b.reservedOutput)} reserved out = {fmtTokens(total)} of {fmtTokens(b.usable)} usable ({fmtCtx(b.contextLimit)} ctx)
          {b.exact ? " · exact" : ""}
        </span>
      </div>
      <div
        className={`bar ${over ? "over" : ""}`}
        title={`Prompt tokens (${b.exact ? "exact, from the loaded model's tokenizer" : "estimated — exact once the model is loaded"}) and reserved output tokens vs. the profile's context`}
      >
        <div className="in" style={{ width: `${inPct}%` }} />
        <div className="out" style={{ width: `${outPct}%` }} />
      </div>
      {prewarm && (
        <div className="faint prewarm-note" style={{ fontSize: 12, color: prewarm.status === "error" ? "var(--err)" : undefined }} title="Settings → Prewarm">
          {prewarm.status === "error" ? "Prewarm failed: " : "Prewarm: "}
          {prewarm.message}
        </div>
      )}
      <div className="row wrap">
        <label className="muted" style={{ fontSize: 12 }}>
          Mode{" "}
          <select
            value={mode}
            onChange={(e) => {
              set({ mode: e.target.value as typeof mode });
              schedulePrewarm(0);
            }}
          >
            <option value="auto">Auto ({b.recommended === "perFile" ? "split per file" : "single call"})</option>
            <option value="single">Single call</option>
            <option value="perFile">One call per file</option>
          </select>
        </label>
        {effective === "perFile" && b.perFile.length > 0 && (
          <span className="faint" style={{ fontSize: 12 }}>
            {b.perFile.length} calls · largest ≈{fmtTokens(Math.max(...b.perFile.map((c) => c.totalTokens)))} tokens
            {b.perFile.some((c) => !c.fits) && <span style={{ color: "var(--err)" }}> · {b.perFile.filter((c) => !c.fits).length} too large</span>}
          </span>
        )}
      </div>
      {over && effective === "single" && <div className="note err">This job does not fit the context window in a single call. Use per-file mode or a larger-context profile.</div>}
    </div>
  );
}

export function SpecPane() {
  const dark = useDarkMode();
  const spec = useStore((s) => s.spec);
  const status = useStore((s) => s.specStatus);
  const expandStatus = useStore((s) => s.expandStatus);
  const stream = useStore((s) => s.claudeStream);
  const activity = useStore((s) => s.claudeActivity);
  const thinking = useStore((s) => s.claudeThinking);
  const info = useStore((s) => s.expandInfo);
  const prepared = useStore((s) => s.prepared);
  const profiles = useStore((s) => s.profiles);
  const implementProfile = useStore((s) => s.implementProfile);
  const runningRun = useStore((s) => s.runs.find((r) => r.status === "running"));
  const root = useStore((s) => s.root);
  const outlinesOn = useStore((s) => s.settings?.outlineContext ?? true);
  const streaming = expandStatus === "running";

  const checkedSet = new Set(useStore.getState().checked);

  return (
    <section className="pane" aria-label="Spec">
      <div className="pane-head">
        <span className="step">2</span>
        <h2>Spec</h2>
        <span className="faint" style={{ fontSize: 12, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
          {info}
        </span>
        <span className="grow" />
        <StatusDot status={status} />
      </div>
      <div className="pane-body">
        {streaming ? (
          <>
            {activity.length > 0 && (
              <div className="faint mono" style={{ fontSize: 11.5 }}>
                {activity.slice(-4).map((a, i) => (
                  <div key={i} style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }} title={a}>
                    🔍 {relToRoot(a, root)}
                  </div>
                ))}
                {activity.length > 4 && <div>… {activity.length} tool calls</div>}
              </div>
            )}
            <pre className="stream" aria-live="polite">
              {stream || (thinking ? `(thinking) ${thinking.slice(-600)}` : info || "Waiting for the planner…")}
            </pre>
          </>
        ) : !spec && status !== "running" ? (
          <div className="empty">
            <div>Let the planner turn an instruction into a spec, or write one yourself.</div>
            <button onClick={() => setSpec(SKELETON)}>Start from the spec template</button>
          </div>
        ) : (
          <div className="editor-box">
            <Editor
              language="markdown"
              theme={dark ? "vs-dark" : "vs"}
              value={spec}
              onChange={(v) => setSpec(v ?? "")}
              options={{ wordWrap: "on", minimap: { enabled: false }, fontSize: 12.5, lineNumbers: "off", scrollBeyondLastLine: false, automaticLayout: true, padding: { top: 6 } }}
            />
          </div>
        )}
        {prepared && !streaming && spec && (
          <>
            <div className="col">
              <span className="section-label">Files for the local model</span>
              <div className="chips">
                {prepared.files.map((f) => {
                  const cls = `chip ${f.error ? "err" : f.target ? (f.exists ? "target" : "new") : "ctx"}`;
                  const why = f.error ?? (f.target ? (f.exists ? "Will be edited" : "Will be created") : checkedSet.has(f.path) ? "Ticked as context" : "Mentioned in the spec → auto-included as context");
                  const body = (
                    <>
                      {f.target ? (f.exists ? "✎" : "+") : "◦"} {f.path}
                      {f.outline && <span className="tag">outline</span>}
                      {f.tokens > 0 && <span className="faint">{fmtTokens(f.tokens)}</span>}
                    </>
                  );
                  if (!outlinesOn || f.target || f.outlineTokens == null) {
                    return (
                      <span key={f.path} className={cls} title={why}>
                        {body}
                      </span>
                    );
                  }
                  const title = f.outline
                    ? `${why}. Sent as an outline (declarations, signatures and docstrings; bodies omitted): ≈${fmtTokens(f.tokens)} of ≈${fmtTokens(f.fullTokens)} tokens. Click to send the whole file.`
                    : `${why}. Sent in full (≈${fmtTokens(f.fullTokens)} tokens). Click to send an outline instead (≈${fmtTokens(f.outlineTokens)} tokens).`;
                  return (
                    <button key={f.path} className={cls} title={title} aria-pressed={!f.outline} onClick={() => toggleFullContext(f.path)}>
                      {body}
                    </button>
                  );
                })}
                {prepared.files.length === 0 && <span className="faint">No files found in the spec's Files section.</span>}
              </div>
            </div>
            {prepared.verify && <Verification v={prepared.verify} />}
            <Budget prepared={prepared} />
            {prepared.warnings.filter((w) => !prepared.budget.warnings.includes(w)).length > 0 && (
              <div className="note warn">
                <ul>
                  {prepared.warnings
                    .filter((w) => !prepared.budget.warnings.includes(w))
                    .map((w, i) => (
                      <li key={i}>{w}</li>
                    ))}
                </ul>
              </div>
            )}
          </>
        )}
      </div>
      <div className="pane-foot">
        {runningRun ? (
          <button className="danger" onClick={() => cancelRun(runningRun.runId)}>
            Cancel run
          </button>
        ) : (
          <button className="primary" disabled={!spec.trim() || streaming || !root} onClick={() => implement()} title="The coder writes the code; then it is verified and reviewed">
            Implement with
          </button>
        )}
        <select
          aria-label="Model for this run"
          value={implementProfile}
          disabled={!!runningRun}
          onChange={(e) => {
            set({ implementProfile: e.target.value });
            schedulePrepare(0);
          }}
        >
          {profiles.map((p) => (
            <option key={p.id} value={p.id}>
              {p.name} · {fmtCtx(p.contextLimit)} ctx
            </option>
          ))}
        </select>
        <span className="faint" style={{ fontSize: 12 }}>
          coder · switches models if needed
        </span>
      </div>
    </section>
  );
}
