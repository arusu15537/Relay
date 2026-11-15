import { useMemo, useState } from "react";
import { cancelExpand, expand, pickProject, refreshFiles, saveSettings, toggleChecked } from "../actions";
import { fmtBytes, fmtCtx } from "../api";
import { set, useStore } from "../store";
import type { FileEntry } from "../types";
import { StatusDot } from "./common";

interface Node {
  name: string;
  path: string;
  dir: boolean;
  size: number;
  children: Node[];
}

function buildTree(files: FileEntry[]): Node {
  const root: Node = { name: "", path: "", dir: true, size: 0, children: [] };
  for (const f of files) {
    const parts = f.path.split("/");
    let cur = root;
    parts.forEach((part, i) => {
      const isFile = i === parts.length - 1;
      const path = parts.slice(0, i + 1).join("/");
      let child = cur.children.find((c) => c.name === part && c.dir === !isFile);
      if (!child) {
        child = { name: part, path, dir: !isFile, size: isFile ? f.size : 0, children: [] };
        cur.children.push(child);
      }
      cur = child;
    });
  }
  const sort = (n: Node) => {
    n.children.sort((a, b) => (a.dir === b.dir ? a.name.localeCompare(b.name) : a.dir ? -1 : 1));
    n.children.forEach(sort);
  };
  sort(root);
  return root;
}

function allFiles(n: Node): string[] {
  return n.dir ? n.children.flatMap(allFiles) : [n.path];
}

function FileTree() {
  const files = useStore((s) => s.files);
  const checked = useStore((s) => s.checked);
  const prepared = useStore((s) => s.prepared);
  const truncated = useStore((s) => s.filesTruncated);
  const [filter, setFilter] = useState("");
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const tree = useMemo(() => buildTree(files), [files]);
  const checkedSet = useMemo(() => new Set(checked), [checked]);
  const specPaths = useMemo(() => new Set((prepared?.files ?? []).map((f) => f.path)), [prepared]);

  const filtered = filter.trim()
    ? files.filter((f) => f.path.toLowerCase().includes(filter.trim().toLowerCase())).slice(0, 500)
    : null;

  const renderNode = (n: Node, depth: number): React.ReactNode[] => {
    if (!n.dir) {
      return [
        <div key={n.path} className={`tree-row ${specPaths.has(n.path) ? "spec-file" : ""}`} style={{ paddingLeft: 6 + depth * 14 }} title={specPaths.has(n.path) ? "Named in the spec (auto-included)" : n.path}>
          <span className="caret" />
          <input type="checkbox" checked={checkedSet.has(n.path)} onChange={(e) => toggleChecked([n.path], e.target.checked)} />
          <span className="name">{n.name}</span>
          <span className="size">{fmtBytes(n.size)}</span>
        </div>,
      ];
    }
    const isOpen = open[n.path] ?? depth < 1;
    const kids = allFiles(n);
    const nChecked = kids.filter((k) => checkedSet.has(k)).length;
    const rows: React.ReactNode[] = [
      <div key={`d:${n.path}`} className="tree-row dir" style={{ paddingLeft: 6 + depth * 14 }}>
        <span className="caret" onClick={() => setOpen((o) => ({ ...o, [n.path]: !isOpen }))}>
          {isOpen ? "▾" : "▸"}
        </span>
        <input
          type="checkbox"
          checked={nChecked > 0 && nChecked === kids.length}
          ref={(el) => {
            if (el) el.indeterminate = nChecked > 0 && nChecked < kids.length;
          }}
          onChange={(e) => toggleChecked(kids, e.target.checked)}
        />
        <span className="name" onClick={() => setOpen((o) => ({ ...o, [n.path]: !isOpen }))}>
          {n.name}
        </span>
        <span className="size">{kids.length}</span>
      </div>,
    ];
    if (isOpen) n.children.forEach((c) => rows.push(...renderNode(c, depth + 1)));
    return rows;
  };

  return (
    <>
      <div className="row">
        <span className="section-label">Context files</span>
        <span className="faint" style={{ fontSize: 12 }}>
          {checked.length} ticked{prepared ? ` · ${prepared.files.filter((f) => !checked.includes(f.path)).length} auto from spec` : ""}
        </span>
        <span style={{ flex: 1 }} />
        {checked.length > 0 && (
          <button className="small ghost" onClick={() => toggleChecked(checked, false)}>
            Clear
          </button>
        )}
        <button className="small ghost" onClick={() => refreshFiles()} title="Refresh file list">
          ⟳
        </button>
      </div>
      <input type="text" placeholder="Filter files…" value={filter} onChange={(e) => setFilter(e.target.value)} />
      <div className="tree" role="tree">
        {filtered
          ? filtered.map((f) => (
              <div key={f.path} className={`tree-row ${specPaths.has(f.path) ? "spec-file" : ""}`}>
                <input type="checkbox" checked={checkedSet.has(f.path)} onChange={(e) => toggleChecked([f.path], e.target.checked)} />
                <span className="name mono">{f.path}</span>
                <span className="size">{fmtBytes(f.size)}</span>
              </div>
            ))
          : tree.children.flatMap((c) => renderNode(c, 0))}
        {truncated && <div className="faint" style={{ padding: 6 }}>File list truncated.</div>}
      </div>
    </>
  );
}

export function InstructionPane() {
  const root = useStore((s) => s.root);
  const instruction = useStore((s) => s.instruction);
  const status = useStore((s) => s.expandStatus);
  const error = useStore((s) => s.expandError);
  const backend = useStore((s) => s.settings?.claudeBackend ?? "cli");
  const planner = useStore((s) => s.settings?.plannerBackend ?? "local");
  const plannerProfile = useStore((s) => s.settings?.plannerProfile ?? "");
  const autopilot = useStore((s) => s.settings?.autopilot ?? false);
  const profiles = useStore((s) => s.profiles);
  const cliPath = useStore((s) => s.boot?.cliPath);
  const hasKey = useStore((s) => s.boot?.hasApiKey);
  const running = status === "running";
  const local = planner === "local";
  const backendProblem = local
    ? profiles.some((p) => p.id === plannerProfile)
      ? null
      : "The planner profile no longer exists. Pick one below or in Settings → Roles."
    : backend === "cli" && !cliPath
      ? "Claude Code CLI not found on PATH. Install it, or switch to the Anthropic API backend in Settings."
      : backend === "api" && !hasKey
        ? "No Anthropic API key saved. Add one in Settings → Claude."
        : null;
  const plannerLabel = local ? (profiles.find((p) => p.id === plannerProfile)?.name ?? plannerProfile) : "Claude";

  return (
    <section className="pane" aria-label="Instruction">
      <div className="pane-head">
        <span className="step">1</span>
        <h2>Instruction</h2>
        <span className="grow" />
        <StatusDot status={status} />
      </div>
      <div className="pane-body">
        {!root ? (
          <div className="empty">
            <div>Choose the project folder you want to change.</div>
            <button className="primary" onClick={() => pickProject()}>
              Choose project…
            </button>
          </div>
        ) : (
          <>
            <textarea
              className="instruction"
              placeholder="What should change? e.g. “add a retry with backoff to the HTTP client”"
              value={instruction}
              onChange={(e) => set({ instruction: e.target.value })}
              onKeyDown={(e) => {
                if (e.key === "Enter" && e.ctrlKey && !running) expand();
              }}
              disabled={running}
            />
            {error && <div className="note err">{error}</div>}
            {backendProblem && <div className="note warn">{backendProblem}</div>}
            <FileTree />
          </>
        )}
      </div>
      <div className="pane-foot">
        {running ? (
          <button className="danger" onClick={() => cancelExpand()}>
            Cancel
          </button>
        ) : (
          <button className="primary" disabled={!root || !instruction.trim()} onClick={() => expand()} title={`The planner (${plannerLabel}) writes the spec`}>
            {local ? "Plan with" : "Expand with Claude"}
          </button>
        )}
        {local && (
          <select
            aria-label="Planner model"
            value={plannerProfile}
            disabled={running}
            onChange={(e) => saveSettings({ plannerProfile: e.target.value })}
            title="The local model that plans (thinking on). Settings → Roles."
          >
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name} · {fmtCtx(p.contextLimit)} ctx
              </option>
            ))}
          </select>
        )}
        <label className="row faint" style={{ fontSize: 12, gap: 4 }} title="Implement, verify and review as soon as the spec is written, without the spec review">
          <input type="checkbox" checked={autopilot} onChange={(e) => saveSettings({ autopilot: e.target.checked })} /> Autopilot
        </label>
        <span className="faint" style={{ fontSize: 12 }}>
          {local ? "" : backend === "cli" ? "Claude Code CLI · " : "Anthropic API · "}
          <span className="kbd">Ctrl</span>+<span className="kbd">Enter</span>
        </span>
      </div>
    </section>
  );
}
