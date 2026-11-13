import Editor from "@monaco-editor/react";
import { cancelRun, implement, schedulePrepare, setSpec } from "../actions";
import { fmtCtx, fmtTokens } from "../api";
import { set, useStore } from "../store";
import type { Prepared } from "../types";
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

function Budget({ prepared }: { prepared: Prepared }) {
  const mode = useStore((s) => s.mode);
  const b = prepared.budget;
  const effective = mode === "auto" ? b.recommended : mode;
  const total = b.single.totalTokens;
  const inPct = Math.min(100, (b.single.inputTokens / b.contextLimit) * 100);
  const outPct = Math.min(100 - inPct, (b.reservedOutput / b.contextLimit) * 100);
  const over = !b.single.fits;
  return (
    <div className="budget">
      <div className="row">
        <span className="section-label">Context budget</span>
        <span className="muted" style={{ fontSize: 12 }}>
          ≈{fmtTokens(b.single.inputTokens)} in + {fmtTokens(b.reservedOutput)} reserved out = {fmtTokens(total)} of {fmtTokens(b.usable)} usable ({fmtCtx(b.contextLimit)} ctx)
        </span>
      </div>
      <div className={`bar ${over ? "over" : ""}`} title="Prompt tokens (estimated) and reserved output tokens vs. the profile's context">
        <div className="in" style={{ width: `${inPct}%` }} />
        <div className="out" style={{ width: `${outPct}%` }} />
      </div>
      <div className="row wrap">
        <label className="muted" style={{ fontSize: 12 }}>
          Mode{" "}
          <select value={mode} onChange={(e) => set({ mode: e.target.value as typeof mode })}>
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
              {stream || (thinking ? `(thinking) ${thinking.slice(-600)}` : "Waiting for Claude…")}
            </pre>
          </>
        ) : !spec && status !== "running" ? (
          <div className="empty">
            <div>Expand an instruction with Claude, or write a spec yourself.</div>
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
                {prepared.files.map((f) => (
                  <span
                    key={f.path}
                    className={`chip ${f.error ? "err" : f.target ? (f.exists ? "target" : "new") : "ctx"}`}
                    title={f.error ?? (f.target ? (f.exists ? "Will be edited" : "Will be created") : checkedSet.has(f.path) ? "Ticked as context" : "Mentioned in the spec → auto-included as context")}
                  >
                    {f.target ? (f.exists ? "✎" : "+") : "◦"} {f.path}
                    {f.tokens > 0 && <span className="faint">{fmtTokens(f.tokens)}</span>}
                  </span>
                ))}
                {prepared.files.length === 0 && <span className="faint">No files found in the spec's Files section.</span>}
              </div>
            </div>
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
          <button className="primary" disabled={!spec.trim() || streaming || !root} onClick={() => implement()}>
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
          switches models if needed
        </span>
      </div>
    </section>
  );
}
