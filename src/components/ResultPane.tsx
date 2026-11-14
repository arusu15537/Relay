import { DiffEditor } from "@monaco-editor/react";
import { useEffect, useMemo, useState } from "react";
import { acceptAll, acceptFile, applyFollowUp, cancelRun, implement, rejectFile, review, setEdited } from "../actions";
import { fmtSecs } from "../api";
import { RunView, set, useStore } from "../store";
import type { AttemptReport, CallReport } from "../types";
import { langFor, StatusDot, useDarkMode } from "./common";

function Live({ run }: { run: RunView }) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(t);
  }, []);
  return (
    <>
      <div className="stats">
        <span>
          <b>{run.callLabel}</b>
        </span>
        <span>
          speed <b>{run.liveTps ? `${run.liveTps.toFixed(1)} tok/s` : "—"}</b>
        </span>
        <span>
          tokens <b>{run.tokens}</b>
        </span>
        <span>
          elapsed <b>{fmtSecs(now - run.startedAt)}</b>
        </span>
      </div>
      {run.notes.length > 0 && (
        <div className="faint" style={{ fontSize: 12 }}>
          {run.notes.map((n, i) => (
            <div key={i}>• {n}</div>
          ))}
        </div>
      )}
      {run.reasoning && (
        <details>
          <summary className="faint">Reasoning ({run.reasoning.length} chars, stripped from the result)</summary>
          <pre className="stream" style={{ maxHeight: 160, minHeight: 0 }}>
            {run.reasoning}
          </pre>
        </details>
      )}
      <pre className="stream" aria-live="polite">
        {run.stream || "…"}
      </pre>
      <div>
        <button className="danger" onClick={() => cancelRun(run.runId)}>
          Cancel
        </button>
      </div>
    </>
  );
}

function AttemptDetails({ a }: { a: AttemptReport }) {
  const failed = a.outcomes.filter((o) => !o.ok);
  return (
    <div className="attempt">
      <div className="row wrap" style={{ fontSize: 12 }}>
        <b>Attempt {a.attempt}</b>
        <span className={a.ok ? "" : "muted"} style={{ color: a.ok ? "var(--ok)" : "var(--err)" }}>
          {a.error ? a.error : a.ok ? "valid" : "validation failed"}
        </span>
        <span className="faint">
          {a.blocks} block(s){a.tokPerSec ? ` · ${a.tokPerSec.toFixed(1)} tok/s` : ""}
          {a.completionTokens ? ` · ${a.completionTokens} tokens` : ""}
          {a.promptTokens ? ` · prompt ${a.promptTokens}` : ""}
          {a.promptTokens && a.cachedTokens != null ? ` (${a.cachedTokens} from cache)` : ""}
          {a.draftTokens ? ` · drafts ${Math.round(((a.draftAccepted ?? 0) / a.draftTokens) * 100)}% accepted` : ""} · {fmtSecs(a.durationMs)}
          {a.finishReason ? ` · ${a.finishReason}` : ""}
        </span>
      </div>
      {(failed.length > 0 || a.parseIssues.length > 0 || a.missingTargets.length > 0) && (
        <ul style={{ margin: "4px 0", paddingLeft: 18, fontSize: 12 }}>
          {a.parseIssues.map((p, i) => (
            <li key={`p${i}`}>{p.message}</li>
          ))}
          {failed.map((o) => (
            <li key={o.index}>
              <span className="mono">{o.path}</span>: {o.message}
            </li>
          ))}
          {a.missingTargets.length > 0 && <li>No successful edit for: {a.missingTargets.join(", ")}</li>}
        </ul>
      )}
      {a.reasoning && (
        <details>
          <summary className="faint">Reasoning (stripped)</summary>
          <pre>{a.reasoning}</pre>
        </details>
      )}
      <details>
        <summary className="faint">Raw model output</summary>
        <pre>{a.rawOutput || "(empty)"}</pre>
      </details>
    </div>
  );
}

function CallsDetails({ calls }: { calls: CallReport[] }) {
  return (
    <details className="calls">
      <summary>
        Model calls ({calls.length}) · {calls.reduce((n, c) => n + c.attempts.length, 0)} attempt(s)
      </summary>
      {calls.map((c) => (
        <div key={c.index} style={{ marginTop: 6 }}>
          <div className="row" style={{ fontSize: 12 }}>
            <b>Call {c.index + 1}</b>
            <span className="mono">{c.target ?? "all files"}</span>
            <StatusDot status={c.ok ? "done" : "error"} label={c.ok ? "ok" : "failed"} />
          </div>
          {c.attempts.map((a) => (
            <AttemptDetails key={a.attempt} a={a} />
          ))}
        </div>
      ))}
    </details>
  );
}

function ReviewSection({ run }: { run: RunView }) {
  const anyAccepted = Object.values(run.decisions).some((d) => d === "accepted");
  const backend = useStore((s) => s.settings?.claudeBackend ?? "cli");
  if (run.reviewStatus === "idle" && !run.review) {
    return anyAccepted ? (
      <div className="row">
        <button onClick={() => review(run.runId)}>Review applied diff with Claude</button>
        <span className="faint" style={{ fontSize: 12 }}>
          checks the accepted changes against the spec ({backend.toUpperCase()})
        </span>
      </div>
    ) : null;
  }
  if (run.reviewStatus === "running") {
    return (
      <div className="col">
        <StatusDot status="running" label="Claude is reviewing the applied diff…" />
        {run.reviewStream && (
          <pre className="stream" style={{ minHeight: 0, maxHeight: 200 }}>
            {run.reviewStream}
          </pre>
        )}
      </div>
    );
  }
  const rv = run.review;
  if (!rv) return null;
  return (
    <div className={`note ${rv.passed ? "ok" : "warn"}`}>
      <div className="row">
        <b>Review: {rv.passed ? "PASS" : "Needs fixes"}</b>
        <span className="grow" style={{ flex: 1 }} />
        {rv.followUpSpec && (
          <button className="small primary" onClick={() => applyFollowUp(run.runId)}>
            Use follow-up spec
          </button>
        )}
      </div>
      <pre className="mono" style={{ whiteSpace: "pre-wrap", margin: "6px 0 0", color: "var(--text)", maxHeight: 220, overflow: "auto" }}>
        {rv.report}
      </pre>
    </div>
  );
}

function Finished({ run }: { run: RunView }) {
  const dark = useDarkMode();
  const runs = useStore((s) => s.runs);
  const compareWith = useStore((s) => s.compareWith);
  const other = runs.find((r) => r.runId === compareWith && r.runId !== run.runId);
  const resp = run.response;
  const result = resp?.result;
  const files = result?.files ?? [];
  const paths = useMemo(() => {
    const set2 = new Set(files.map((f) => f.path));
    other?.response?.result.files.forEach((f) => set2.add(f.path));
    return [...set2].sort();
  }, [files, other]);
  const [sel, setSel] = useState<string | null>(null);
  const selected = sel && paths.includes(sel) ? sel : (paths[0] ?? null);
  const file = files.find((f) => f.path === selected);
  const otherFile = other?.response?.result.files.find((f) => f.path === selected);
  const decision = selected ? run.decisions[selected] : undefined;
  const pending = Object.values(run.decisions).filter((d) => d === "pending").length;
  const failedCalls = result?.calls.filter((c) => !c.ok) ?? [];

  if (!result) {
    return <div className="note err">{run.error ?? "The run failed before producing a result."}</div>;
  }

  const original = other ? (file?.proposed ?? file?.original ?? "") : (file?.original ?? "");
  const modified = other ? (otherFile?.proposed ?? otherFile?.original ?? file?.original ?? "") : (run.edited[selected ?? ""] ?? file?.proposed ?? "");

  return (
    <>
      <div className="stats">
        <span>
          model <b>{run.profileName}</b>
        </span>
        <span>
          mode <b>{result.mode === "perFile" ? "per file" : "single"}</b>
        </span>
        <span>
          calls <b>{result.calls.length}</b>
        </span>
        <span>
          avg speed <b>{result.avgTokPerSec ? `${result.avgTokPerSec.toFixed(1)} tok/s` : "—"}</b>
        </span>
        <span>
          time <b>{fmtSecs(result.durationMs)}</b>
        </span>
        <span style={{ color: result.validationOk ? "var(--ok)" : "var(--err)" }}>{result.validationOk ? "✓ all edits valid" : result.cancelled ? "cancelled" : "✗ validation failed"}</span>
      </div>
      {run.error && !result.cancelled && <div className="note err">{run.error}</div>}
      {failedCalls.length > 0 && !result.cancelled && (
        <div className="note err">
          {failedCalls.length} call(s) failed after the automatic retry. Valid edits are shown below; the failed ones are listed under “Model calls”.
          {resp?.fallbackProfile && <> You can retry with the fallback model from the footer.</>}
        </div>
      )}
      {result.warnings.length > 0 && (
        <details className="calls">
          <summary>Warnings ({result.warnings.length})</summary>
          <ul style={{ margin: "4px 0", paddingLeft: 18, fontSize: 12 }}>
            {result.warnings.map((w, i) => (
              <li key={i}>{w}</li>
            ))}
          </ul>
        </details>
      )}
      {other && (
        <div className="compare-grid">
          {[run, other].map((r, i) => (
            <div key={r.runId} className="compare-card">
              <div className="row">
                <b>{i === 0 ? "Left" : "Right"}: {r.profileName}</b>
                <StatusDot status={r.status} label={r.response?.result.validationOk ? "valid" : "invalid"} />
              </div>
              <div className="stats">
                <span>{r.response?.result.avgTokPerSec ? `${r.response.result.avgTokPerSec.toFixed(1)} tok/s` : "—"}</span>
                <span>{r.response ? fmtSecs(r.response.result.durationMs) : ""}</span>
                <span>{r.response?.result.files.length ?? 0} files</span>
                <span>{r.response?.result.calls.reduce((n, c) => n + c.attempts.length, 0) ?? 0} attempts</span>
              </div>
            </div>
          ))}
        </div>
      )}
      {paths.length === 0 ? (
        <div className="empty">No file changes were produced.</div>
      ) : (
        <>
          <div className="file-list">
            {paths.map((p) => {
              const f = files.find((x) => x.path === p);
              const d = run.decisions[p];
              return (
                <div key={p} className={`file-item ${p === selected ? "sel" : ""}`} onClick={() => setSel(p)}>
                  {f && f.original === null && <span className="badge new">new</span>}
                  <span className="path">{p}</span>
                  {!f && <span className="badge">only in {other?.profileName}</span>}
                  {d && d !== "pending" && <span className={`badge ${d}`}>{d}</span>}
                  {d === "pending" && !other && (
                    <>
                      <button className="small ok" onClick={(e) => (e.stopPropagation(), acceptFile(run.runId, p))}>
                        Accept
                      </button>
                      <button className="small danger" onClick={(e) => (e.stopPropagation(), rejectFile(run.runId, p))}>
                        Reject
                      </button>
                    </>
                  )}
                </div>
              );
            })}
          </div>
          <div className="row">
            <span className="muted" style={{ fontSize: 12 }}>
              {other ? `Diff: ${run.profileName} (left) vs ${other.profileName} (right)` : decision === "pending" ? "Original (left) · proposed (right, editable before accepting)" : "Original (left) · applied (right)"}
            </span>
            <span style={{ flex: 1 }} />
            {!other && pending > 0 && (
              <button className="small primary" onClick={() => acceptAll(run.runId)}>
                Accept all ({pending})
              </button>
            )}
          </div>
          <div className="diff-box">
            {selected && (
              <DiffEditor
                key={`${run.runId}:${other?.runId ?? ""}:${selected}`}
                original={original}
                modified={modified}
                language={langFor(selected)}
                theme={dark ? "vs-dark" : "vs"}
                originalModelPath={`inmemory://a/${run.runId}/${other?.runId ?? "o"}/${selected}`}
                modifiedModelPath={`inmemory://b/${run.runId}/${other?.runId ?? "m"}/${selected}`}
                keepCurrentOriginalModel
                keepCurrentModifiedModel
                options={{
                  renderSideBySide: true,
                  useInlineViewWhenSpaceIsLimited: false,
                  readOnly: !!other || decision !== "pending",
                  originalEditable: false,
                  automaticLayout: true,
                  minimap: { enabled: false },
                  fontSize: 12,
                  scrollBeyondLastLine: false,
                  renderOverviewRuler: false,
                }}
                onMount={(editor) => {
                  const mod = editor.getModifiedEditor();
                  mod.onDidChangeModelContent(() => {
                    if (!other && run.decisions[selected] === "pending") setEdited(run.runId, selected, mod.getValue());
                  });
                }}
              />
            )}
          </div>
        </>
      )}
      <CallsDetails calls={result.calls} />
      <ReviewSection run={run} />
    </>
  );
}

export function ResultPane() {
  const runs = useStore((s) => s.runs);
  const activeRun = useStore((s) => s.activeRun);
  const compareWith = useStore((s) => s.compareWith);
  const profiles = useStore((s) => s.profiles);
  const spec = useStore((s) => s.spec);
  const run = runs.find((r) => r.runId === activeRun) ?? runs[runs.length - 1];
  const anyRunning = runs.some((r) => r.status === "running");
  const [rerunWith, setRerunWith] = useState("");
  const fallback = run?.response?.fallbackProfile;
  const rerunTarget = rerunWith || profiles.find((p) => p.id !== run?.profileId)?.id || "";

  return (
    <section className="pane" aria-label="Result">
      <div className="pane-head">
        <span className="step">3</span>
        <h2>Result</h2>
        <span className="grow" />
        <StatusDot status={run?.status ?? "idle"} />
      </div>
      {runs.length > 0 && (
        <div className="tabs" role="tablist">
          {runs.map((r) => (
            <button key={r.runId} role="tab" className={`tab ${r.runId === run?.runId ? "active" : ""}`} onClick={() => set({ activeRun: r.runId, compareWith: null })}>
              <span className={`dot ${r.status}`} />
              {r.profileName}
              {r.response?.result.avgTokPerSec && <span className="tps">{r.response.result.avgTokPerSec.toFixed(0)} t/s</span>}
            </button>
          ))}
        </div>
      )}
      <div className="pane-body">
        {!run ? (
          <div className="empty">
            <div>The local model's changes appear here as diffs.</div>
            <div className="faint">Accept or reject each file; accepted files are written to the project.</div>
          </div>
        ) : run.status === "running" ? (
          <Live run={run} />
        ) : (
          <Finished run={run} />
        )}
      </div>
      {run && run.status !== "running" && (
        <div className="pane-foot">
          {fallback && !run.response?.result.validationOk && (
            <button className="primary" disabled={anyRunning} onClick={() => implement(fallback, run.spec)}>
              Retry with {profiles.find((p) => p.id === fallback)?.name ?? fallback}
            </button>
          )}
          <button disabled={anyRunning || !(run.spec || spec)} onClick={() => implement(rerunTarget, run.spec || spec)}>
            Re-run with
          </button>
          <select aria-label="Re-run with model" value={rerunTarget} onChange={(e) => setRerunWith(e.target.value)}>
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
          {runs.length > 1 && (
            <>
              <span className="faint" style={{ marginLeft: 8 }}>
                Compare with
              </span>
              <select aria-label="Compare with run" value={compareWith ?? ""} onChange={(e) => set({ compareWith: e.target.value || null })}>
                <option value="">—</option>
                {runs
                  .filter((r) => r.runId !== run.runId && r.response)
                  .map((r) => (
                    <option key={r.runId} value={r.runId}>
                      {r.profileName} (run {runs.indexOf(r) + 1})
                    </option>
                  ))}
              </select>
            </>
          )}
        </div>
      )}
    </section>
  );
}
