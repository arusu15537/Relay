import { isBusy, newJob, pickProject, stopServer, switchProfile } from "../actions";
import { set, useStore } from "../store";
import { StatusDot } from "./common";

function ModelSwitcher() {
  const profiles = useStore((s) => s.profiles);
  const server = useStore((s) => s.server);
  const active = useStore((s) => s.settings?.activeProfile ?? "");
  const busy = useStore((s) => s.expandStatus === "running" || s.runs.some((r) => r.status === "running"));
  const state = server?.state ?? "unloaded";
  // The status refers to the profile that is loaded/loading; the dropdown shows the selection.
  const shown = server?.profileId ?? active;
  return (
    <div className="switcher" title={server?.hint ?? server?.message ?? ""}>
      <StatusDot status={state} label={state === "ready" && server?.external ? "connected" : undefined} />
      <select
        aria-label="Local model"
        value={shown}
        disabled={busy && state !== "error"}
        onChange={(e) => switchProfile(e.target.value)}
      >
        {profiles.map((p) => (
          <option key={p.id} value={p.id}>
            {p.name}
            {p.kind === "external" ? " (external)" : ""}
          </option>
        ))}
      </select>
      {state === "loading" && server?.progress != null && (
        <div className="progress" aria-label="Load progress">
          <div style={{ width: `${Math.round(server.progress * 100)}%` }} />
        </div>
      )}
      <span className="msg">{server?.message}</span>
      {state === "unloaded" || state === "error" ? (
        <button className="small" disabled={busy} onClick={() => switchProfile(shown)} title="Load this model">
          Load
        </button>
      ) : (
        <button className="small ghost" disabled={busy} onClick={() => stopServer()} title={server?.external ? "Disconnect" : "Unload the model and free VRAM"}>
          Unload
        </button>
      )}
    </div>
  );
}

function Vram() {
  const gpu = useStore((s) => s.gpu);
  if (!gpu) return null;
  const [used, total] = gpu;
  const high = used / total > 0.94;
  return (
    <span className={`vram ${high ? "high" : ""}`} title="GPU memory in use (all processes)">
      VRAM {(used / 1024).toFixed(1)} / {(total / 1024).toFixed(1)} GB
    </span>
  );
}

export function Toolbar() {
  const root = useStore((s) => s.root);
  const git = useStore((s) => s.git);
  const logOpen = useStore((s) => s.logOpen);
  const name = root ? root.split(/[\\/]/).filter(Boolean).pop() : "";
  return (
    <div className="toolbar">
      <div className="brand">
        <span className="brand-mark" />
        Relay
      </div>
      <button className="project-btn" onClick={() => pickProject()} title={root || "Choose the target project folder"}>
        📁 {name || "Choose project…"}
      </button>
      {git?.isRepo && (
        <span className="muted" title={git.changes.slice(0, 20).join("\n")} style={{ fontSize: 12 }}>
          git: {git.branch}
          {git.dirty ? ` · ${git.changes.length} uncommitted` : " · clean"}
        </span>
      )}
      <button className="small ghost" onClick={() => newJob()} disabled={isBusy()} title="Start a new instruction">
        New
      </button>
      <div className="spacer" />
      <ModelSwitcher />
      <Vram />
      <button className="small" onClick={() => set({ historyOpen: true })}>
        History
      </button>
      <button className={`small ${logOpen ? "primary" : ""}`} onClick={() => set({ logOpen: !logOpen })} title="Show the llama-server log">
        Log
      </button>
      <button className="small" onClick={() => set({ settingsOpen: true })}>
        Settings
      </button>
    </div>
  );
}
