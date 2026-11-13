import { useEffect, useState } from "react";
import { loadJob } from "../actions";
import { api, errText, fmtSecs } from "../api";
import { ask, set, toast, useStore } from "../store";
import type { JobSummary } from "../types";
import { StatusDot } from "./common";

export function HistoryDrawer() {
  const open = useStore((s) => s.historyOpen);
  const [query, setQuery] = useState("");
  const [jobs, setJobs] = useState<JobSummary[]>([]);
  const reload = async (q = query) => {
    try {
      setJobs(await api.historyList(q));
    } catch (e) {
      toast(errText(e), "error");
    }
  };
  useEffect(() => {
    if (open) reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);
  if (!open) return null;
  return (
    <div className="overlay right" onMouseDown={(e) => e.target === e.currentTarget && set({ historyOpen: false })}>
      <div className="drawer" role="dialog" aria-label="History">
        <div className="dialog" style={{ border: "none", borderRadius: 0, boxShadow: "none", maxHeight: "100vh", height: "100%", maxWidth: "100%" }}>
          <div className="dhead">
            <h3>History</h3>
            <button className="small ghost" onClick={() => set({ historyOpen: false })}>
              ✕
            </button>
          </div>
          <div style={{ padding: "8px 14px", borderBottom: "1px solid var(--border)" }}>
            <input
              type="text"
              placeholder="Search instructions, specs, projects…"
              style={{ width: "100%" }}
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                reload(e.target.value);
              }}
            />
          </div>
          <div style={{ overflow: "auto", flex: 1 }}>
            {jobs.length === 0 && <div className="empty">No runs yet.</div>}
            {jobs.map((j) => (
              <div key={j.id} className="history-item" onClick={() => loadJob(j.id)}>
                <div className="row">
                  <b style={{ flex: 1, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{j.instruction || "(spec written by hand)"}</b>
                  <span className="faint" style={{ fontSize: 11.5 }}>
                    {new Date(j.createdAt).toLocaleString()}
                  </span>
                  <button
                    className="small ghost"
                    title="Delete from history"
                    onClick={async (e) => {
                      e.stopPropagation();
                      if ((await ask("Delete job?", "This removes the job and its runs from history. Files already written are not touched.", [{ id: "del", label: "Delete", kind: "danger" }, { id: "cancel", label: "Cancel" }])) === "del") {
                        await api.historyDelete(j.id);
                        reload();
                      }
                    }}
                  >
                    🗑
                  </button>
                </div>
                <div className="faint mono" style={{ fontSize: 11 }}>
                  {j.projectRoot}
                </div>
                <div className="col" style={{ marginTop: 4, gap: 2 }}>
                  {j.runs.map((r) => (
                    <div key={r.id} className="row" style={{ fontSize: 12 }}>
                      <StatusDot status={r.status === "ok" ? "done" : r.status === "cancelled" ? "idle" : "error"} label={r.profileName} />
                      <span className="faint">
                        {r.avgTokPerSec ? `${r.avgTokPerSec.toFixed(1)} tok/s · ` : ""}
                        {fmtSecs(r.durationMs)} · {r.files} file(s), {r.accepted} accepted, {r.rejected} rejected
                      </span>
                    </div>
                  ))}
                  {j.runs.length === 0 && <span className="faint" style={{ fontSize: 12 }}>spec only, not implemented</span>}
                </div>
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
