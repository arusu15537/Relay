import Editor from "@monaco-editor/react";
import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";
import { saveProfiles, saveSettings, schedulePrepare, schedulePrewarm, switchProfile } from "../actions";
import { api, errText, fmtBytes, fmtCtx } from "../api";
import { ask, set, toast, useStore } from "../store";
import type { HfFile, ModelFileStatus, Profile, ProfileTest, TemplateInfo } from "../types";
import { Field, useDarkMode } from "./common";

type Tab = "general" | "roles" | "claude" | "models" | "templates";

function DownloadProgress({ id }: { id: string }) {
  const p = useStore((s) => s.downloads[id]);
  if (!p || p.phase === "done") return null;
  const pct = p.total ? Math.min(100, (p.done / p.total) * 100) : 0;
  return (
    <div className="col" style={{ gap: 3 }}>
      <div className="progress" style={{ width: "100%" }}>
        <div style={{ width: `${pct}%` }} />
      </div>
      <span className="faint" style={{ fontSize: 12 }}>
        {p.phase === "hashing-partial" ? "Checking the partial download…" : p.phase} · {fmtBytes(p.done)} / {fmtBytes(p.total)}
        {p.bytesPerSec > 0 && ` · ${fmtBytes(p.bytesPerSec)}/s`}
      </span>
    </div>
  );
}

function General() {
  const s = useStore((st) => st.settings)!;
  const boot = useStore((st) => st.boot);
  const profiles = useStore((st) => st.profiles);
  const [busy, setBusy] = useState(false);
  const browseDir = async () => {
    const d = await open({ directory: true, title: "Models folder" });
    if (typeof d === "string") saveSettings({ modelsDir: d });
  };
  const browseExe = async () => {
    const f = await open({ filters: [{ name: "llama-server", extensions: ["exe"] }], title: "Locate llama-server.exe (CUDA build)" });
    if (typeof f === "string") saveSettings({ llamaServerPath: f });
  };
  const getLlama = async () => {
    setBusy(true);
    try {
      const rel = await api.llamaRelease();
      const parts = [rel.serverZip, ...(rel.cudartZip ? [rel.cudartZip] : [])];
      const total = parts.reduce((n, p) => n + p[2], 0);
      const choice = await ask(
        `Download llama.cpp ${rel.tag}?`,
        `Relay will download these files from github.com/ggml-org/llama.cpp (${fmtBytes(total)} total) and extract them to ${boot?.appDir}\\llama.cpp.`,
        [
          { id: "go", label: `Download ${fmtBytes(total)}`, kind: "primary" },
          { id: "cancel", label: "Cancel" },
        ],
        parts.map((p) => `${p[0]}  (${fmtBytes(p[2])})`),
      );
      if (choice !== "go") return;
      const exe = await api.downloadLlama(rel);
      saveSettings({ llamaServerPath: exe });
      toast(`llama.cpp ${rel.tag} installed`, "ok");
    } catch (e) {
      toast(errText(e), "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="form">
      <Field label="Models folder" hint="Default GGUF location for profiles without an explicit file path.">
        <div className="row">
          <input type="text" style={{ flex: 1 }} value={s.modelsDir} onChange={(e) => saveSettings({ modelsDir: e.target.value })} />
          <button onClick={browseDir}>Browse…</button>
        </div>
      </Field>
      <Field label="llama-server.exe" hint={s.llamaServerPath ? undefined : boot?.llamaServer ? `Auto-detected: ${boot.llamaServer}` : "Not found. Pick a CUDA build of llama-server.exe or download one."}>
        <div className="row">
          <input type="text" style={{ flex: 1 }} placeholder="auto-detect" value={s.llamaServerPath} onChange={(e) => saveSettings({ llamaServerPath: e.target.value })} />
          <button onClick={browseExe}>Browse…</button>
          <button onClick={getLlama} disabled={busy}>
            Download…
          </button>
        </div>
        <DownloadProgress id="llama" />
      </Field>
      <Field label="Managed server port" hint="Relay picks the next free port if this one is taken.">
        <input type="number" value={s.serverPort} min={1024} max={65000} onChange={(e) => saveSettings({ serverPort: Number(e.target.value) })} />
      </Field>
      <Field label="Load timeout (s)">
        <input type="number" value={s.loadTimeoutSecs} min={30} onChange={(e) => saveSettings({ loadTimeoutSecs: Number(e.target.value) })} />
      </Field>
      <Field
        label="Context outlines"
        hint="Large read-only context files (ticked or mentioned in the spec, not edited) go to the local model as outlines: imports, declarations, signatures and docstrings, with function bodies omitted. Click a file chip in the Spec pane to send that file in full."
      >
        <label className="row">
          <input
            type="checkbox"
            checked={s.outlineContext}
            onChange={(e) => {
              void saveSettings({ outlineContext: e.target.checked }).then(() => schedulePrepare(0));
            }}
          />{" "}
          Send large context files as outlines
        </label>
      </Field>
      <Field
        label="Prewarm"
        hint="While you review the spec, evaluate everything before it (instructions, read-only context, files to edit) into the loaded model's prompt cache, so a run only has to process the spec. Uses the GPU for a few seconds after you pick files; never loads a model. Works with llama-server (managed, or external)."
      >
        <label className="row">
          <input
            type="checkbox"
            checked={s.prewarmPrompt}
            onChange={(e) => {
              void saveSettings({ prewarmPrompt: e.target.checked }).then(() => schedulePrewarm(0));
            }}
          />{" "}
          Prewarm the prompt cache while I review the spec
        </label>
      </Field>
      <Field label="Auto-fallback" hint="If a model's output fails edit-format validation twice, offer to retry the same spec with the fallback profile.">
        <div className="row">
          <label className="row">
            <input type="checkbox" checked={s.autoFallback} onChange={(e) => saveSettings({ autoFallback: e.target.checked })} /> Offer fallback
          </label>
          <select value={s.fallbackProfile} onChange={(e) => saveSettings({ fallbackProfile: e.target.value })}>
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </div>
      </Field>
      <Field label="Data folder" hint="Settings, profiles, history.db, templates and logs.">
        <div className="row">
          <span className="mono">{boot?.appDir}</span>
          <button className="small" onClick={() => boot && api.openInExplorer(boot.appDir)}>
            Open
          </button>
        </div>
      </Field>
    </div>
  );
}


function Roles() {
  const s = useStore((st) => st.settings)!;
  const profiles = useStore((st) => st.profiles);
  const planner = profiles.find((p) => p.id === s.plannerProfile);
  const coder = profiles.find((p) => p.id === s.coderProfile);
  const swaps = s.plannerBackend === "local" && planner && coder && planner.id !== coder.id;
  const loadSecs = (id: string | undefined) => (id && s.loadStats[id] ? `≈${Math.round(s.loadStats[id])}s` : "unmeasured");
  const profileSelect = (value: string, onChange: (id: string) => void, label: string) => (
    <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)}>
      {profiles.map((p) => (
        <option key={p.id} value={p.id}>
          {p.name} · {fmtCtx(p.contextLimit)} ctx
        </option>
      ))}
    </select>
  );
  return (
    <div className="form">
      <Field label="Planner" hint="Writes the spec from your instruction, and reviews the verified change. A local planner runs with thinking on.">
        <div className="row wrap">
          <label className="row">
            <input type="radio" checked={s.plannerBackend === "local"} onChange={() => saveSettings({ plannerBackend: "local" })} /> Local model
          </label>
          <label className="row">
            <input type="radio" checked={s.plannerBackend === "claude"} onChange={() => saveSettings({ plannerBackend: "claude" })} /> Claude (see the Claude tab)
          </label>
        </div>
      </Field>
      {s.plannerBackend === "local" && (
        <>
          <Field label="Planner model">{profileSelect(s.plannerProfile, (id) => saveSettings({ plannerProfile: id }), "Planner model")}</Field>
          <Field
            label="Planner effort"
            hint="For models with a reasoning-effort switch (gpt-oss). Qwen models just think with thinking enabled."
          >
            <select value={s.plannerEffort} onChange={(e) => saveSettings({ plannerEffort: e.target.value })}>
              {["low", "medium", "high"].map((x) => (
                <option key={x}>{x}</option>
              ))}
            </select>
          </Field>
          <Field label="Project tools" hint="Lets the planner call read_file, list_files and search (read-only, inside the project) before it writes the spec or review.">
            <label className="row">
              <input type="checkbox" checked={s.plannerTools} onChange={(e) => saveSettings({ plannerTools: e.target.checked })} /> Allow read-only tools
            </label>
          </Field>
        </>
      )}
      <Field label="Coder model" hint="Writes the code (thinking off). The default for new runs; the Spec pane can pick another per run.">
        {profileSelect(
          s.coderProfile,
          (id) => {
            saveSettings({ coderProfile: id });
            set({ implementProfile: id });
            schedulePrepare(0);
          },
          "Coder model",
        )}
      </Field>
      {swaps && (
        <div className="note warn">
          Planner and coder use different models, and only one fits in VRAM: Relay unloads one and loads the other at every hand-off (planner {loadSecs(planner?.id)}, coder {loadSecs(coder?.id)} to load).
        </div>
      )}
      <Field
        label="Verification"
        hint="The change is applied to a staging copy of the project (in the data folder), where the spec's build / test / run commands run. A failure goes back to the coder with the condensed output."
      >
        <label className="row">
          <input
            type="checkbox"
            checked={s.verifyEnabled}
            onChange={(e) => {
              void saveSettings({ verifyEnabled: e.target.checked }).then(() => schedulePrepare(0));
            }}
          />{" "}
          Build, test and run the change before showing it
        </label>
      </Field>
      <Field label="Fix rounds" hint="How often the coder may try to fix a failing command.">
        <input type="number" min={0} max={8} value={s.maxFixRounds} onChange={(e) => saveSettings({ maxFixRounds: Math.max(0, Number(e.target.value)) })} />
      </Field>
      <Field label="Timeouts (s)" hint="Setup, build and test commands fail when they take longer. A run command still running at its timeout counts as started fine (games, servers).">
        <div className="row wrap">
          <label className="row" style={{ gap: 4 }}>
            <span className="faint">build/test</span>
            <input type="number" min={10} style={{ width: 80 }} value={s.buildTimeoutSecs} onChange={(e) => saveSettings({ buildTimeoutSecs: Number(e.target.value) })} />
          </label>
          <label className="row" style={{ gap: 4 }}>
            <span className="faint">run</span>
            <input type="number" min={1} style={{ width: 80 }} value={s.runTimeoutSecs} onChange={(e) => saveSettings({ runTimeoutSecs: Number(e.target.value) })} />
          </label>
        </div>
      </Field>
      <Field label="Review" hint="The planner reviews the diff and the verification results against the spec. If it finds problems, its follow-up spec goes back to the coder.">
        <div className="row wrap">
          <label className="row">
            <input type="checkbox" checked={s.loopReview} onChange={(e) => saveSettings({ loopReview: e.target.checked })} /> Review before showing the result
          </label>
          <label className="row" style={{ gap: 4 }}>
            <span className="faint">follow-up passes</span>
            <input type="number" min={0} max={4} style={{ width: 60 }} value={s.followUpRounds} onChange={(e) => saveSettings({ followUpRounds: Math.max(0, Number(e.target.value)) })} />
          </label>
        </div>
      </Field>
      <Field label="Autopilot" hint="Start implementing as soon as the spec is written, without the spec review. The diff still waits for you to accept it.">
        <label className="row">
          <input type="checkbox" checked={s.autopilot} onChange={(e) => saveSettings({ autopilot: e.target.checked })} /> Skip the spec review
        </label>
      </Field>
    </div>
  );
}

function Claude() {
  const s = useStore((st) => st.settings)!;
  const boot = useStore((st) => st.boot);
  const [key, setKey] = useState("");
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const test = async () => {
    setTesting(true);
    setResult(null);
    try {
      setResult(await api.testClaude());
    } catch (e) {
      setResult(`✗ ${errText(e)}`);
    } finally {
      setTesting(false);
    }
  };
  return (
    <div className="form">
      <Field label="Backend">
        <div className="row">
          <label className="row">
            <input type="radio" checked={s.claudeBackend === "cli"} onChange={() => saveSettings({ claudeBackend: "cli" })} /> Claude Code CLI (subscription)
          </label>
          <label className="row">
            <input type="radio" checked={s.claudeBackend === "api"} onChange={() => saveSettings({ claudeBackend: "api" })} /> Anthropic API
          </label>
        </div>
      </Field>
      {s.claudeBackend === "cli" ? (
        <>
          <Field label="claude on PATH" hint={boot?.cliPath ? undefined : "Install Claude Code and sign in once by running `claude` in a terminal, then restart Relay."}>
            <span className={boot?.cliPath ? "mono" : ""} style={{ color: boot?.cliPath ? undefined : "var(--err)" }}>
              {boot?.cliPath ?? "Not found"}
            </span>
          </Field>
          <Field label="Explore project" hint="Lets Claude use read-only Read/Glob/Grep in the project folder while writing the spec (better specs, a bit slower).">
            <label className="row">
              <input type="checkbox" checked={s.claudeCliExplore} onChange={(e) => saveSettings({ claudeCliExplore: e.target.checked })} /> Allow read-only tools
            </label>
          </Field>
        </>
      ) : (
        <Field label="API key" hint="Stored in Windows Credential Manager (never in a config file).">
          <div className="row">
            <input type="password" style={{ flex: 1 }} placeholder={boot?.hasApiKey ? "•••••••• (saved)" : "sk-ant-…"} value={key} onChange={(e) => setKey(e.target.value)} />
            <button
              onClick={async () => {
                try {
                  await api.setApiKey(key);
                  set((st) => ({ boot: st.boot ? { ...st.boot, hasApiKey: !!key.trim() } : st.boot }));
                  setKey("");
                  toast(key.trim() ? "API key saved to Credential Manager" : "API key removed", "ok");
                } catch (e) {
                  toast(errText(e), "error");
                }
              }}
            >
              {key.trim() ? "Save" : "Remove"}
            </button>
          </div>
        </Field>
      )}
      <Field label="Model ID" hint="Used by both backends (passed to the CLI as --model).">
        <input type="text" value={s.claudeModel} onChange={(e) => saveSettings({ claudeModel: e.target.value })} />
      </Field>
      <Field label="Effort" hint="Higher effort gives better specs and takes longer.">
        <select value={s.claudeEffort} onChange={(e) => saveSettings({ claudeEffort: e.target.value })}>
          {["low", "medium", "high", "xhigh", "max"].map((x) => (
            <option key={x}>{x}</option>
          ))}
        </select>
      </Field>
      <Field label="Test">
        <div className="row">
          <button onClick={test} disabled={testing}>
            {testing ? "Testing…" : "Test backend"}
          </button>
          {result && <span style={{ color: result.startsWith("✗") ? "var(--err)" : "var(--ok)" }}>{result}</span>}
        </div>
      </Field>
    </div>
  );
}

function DownloadPicker({ profile, onClose }: { profile: Profile; onClose: () => void }) {
  const modelsDir = useStore((s) => s.settings?.modelsDir);
  const [files, setFiles] = useState<HfFile[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sel, setSel] = useState(profile.hfFile);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    api.hfList(profile.hfRepo).then(setFiles, (e) => setError(errText(e)));
  }, [profile.hfRepo]);
  const chosen = files?.find((f) => f.path === sel);
  const go = async () => {
    if (!chosen) return;
    const ok = await ask(
      "Download model?",
      `Download ${chosen.path} (${fmtBytes(chosen.size)}) from huggingface.co/${profile.hfRepo} into ${modelsDir}?`,
      [
        { id: "go", label: `Download ${fmtBytes(chosen.size)}`, kind: "primary" },
        { id: "cancel", label: "Cancel" },
      ],
      [`SHA256: ${chosen.sha256 ?? "not published"}`, "Interrupted downloads resume from where they stopped."],
    );
    if (ok !== "go") return;
    setBusy(true);
    try {
      const out = await api.downloadModel(profile.id, chosen.path, chosen.size, chosen.sha256);
      if (out.cancelled) toast("Download paused; start it again to resume.");
      else {
        toast(`Downloaded ${chosen.path}${out.verified ? " (SHA256 verified)" : ""}`, "ok");
        set({ profiles: (await api.getAppState()).profiles });
        onClose();
      }
    } catch (e) {
      toast(errText(e), "error");
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && !busy && onClose()}>
      <div className="dialog" style={{ width: 620 }}>
        <div className="dhead">
          <h3>Download for {profile.name}</h3>
        </div>
        <div className="dbody">
          <span className="muted">huggingface.co/{profile.hfRepo}</span>
          {error && <div className="note err">{error}</div>}
          {!files && !error && <span className="faint">Loading file list…</span>}
          {files && (
            <div className="file-list" style={{ maxHeight: 320 }}>
              {files.map((f) => (
                <label key={f.path} className={`file-item ${f.path === sel ? "sel" : ""}`}>
                  <input type="radio" checked={f.path === sel} onChange={() => setSel(f.path)} />
                  <span className="path">{f.path}</span>
                  <span className="faint">{fmtBytes(f.size)}</span>
                  {f.path === profile.hfFile && <span className="badge new">profile default</span>}
                </label>
              ))}
            </div>
          )}
          <DownloadProgress id={`model:${profile.id}`} />
        </div>
        <div className="dfoot">
          {busy ? (
            <button className="danger" onClick={() => api.cancelJob(`model:${profile.id}`)}>
              Pause / cancel
            </button>
          ) : (
            <button onClick={onClose}>Close</button>
          )}
          <button className="primary" disabled={!chosen || busy} onClick={go}>
            Download…
          </button>
        </div>
      </div>
    </div>
  );
}

function num(v: string): number {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}

function ProfileEditor({ p, onChange }: { p: Profile; onChange: (p: Profile) => void }) {
  const modelsDir = useStore((s) => s.settings?.modelsDir ?? "");
  const [status, setStatus] = useState<ModelFileStatus | null>(null);
  const [testRes, setTestRes] = useState<ProfileTest | string | null>(null);
  const [testing, setTesting] = useState(false);
  const [picker, setPicker] = useState(false);
  const [epKey, setEpKey] = useState("");
  useEffect(() => {
    setTestRes(null);
    if (p.kind === "managed") api.modelFileStatus(p.id).then(setStatus, () => setStatus(null));
  }, [p.id, p.kind, p.ggufPath, p.hfFile]);
  const up = (patch: Partial<Profile>) => onChange({ ...p, ...patch });
  const browse = async () => {
    const f = await open({ filters: [{ name: "GGUF", extensions: ["gguf"] }], title: `GGUF for ${p.name}` });
    if (typeof f === "string") up({ ggufPath: f });
  };
  const test = async () => {
    setTesting(true);
    setTestRes(null);
    try {
      setTestRes(await api.testProfile(p.id));
    } catch (e) {
      setTestRes(errText(e));
    } finally {
      setTesting(false);
    }
  };
  return (
    <div className="col" style={{ gap: 12 }}>
      <div className="form">
        <Field label="Name">
          <input type="text" value={p.name} onChange={(e) => up({ name: e.target.value })} />
        </Field>
        <Field label="Role">
          <input type="text" value={p.role} onChange={(e) => up({ role: e.target.value })} />
        </Field>
        <Field label="Source">
          <div className="row">
            <label className="row">
              <input type="radio" checked={p.kind === "managed"} onChange={() => up({ kind: "managed" })} /> Managed llama-server
            </label>
            <label className="row">
              <input type="radio" checked={p.kind === "external"} onChange={() => up({ kind: "external" })} /> External endpoint
            </label>
          </div>
        </Field>
        {p.kind === "managed" ? (
          <>
            <Field
              label="GGUF file"
              hint={
                status
                  ? status.exists
                    ? `Found · ${fmtBytes(status.size)}${status.expectedSize && status.size !== status.expectedSize ? ` (expected ${fmtBytes(status.expectedSize)})` : ""}`
                    : `Missing: ${status.path}${status.partial ? ` · partial download ${fmtBytes(status.partial)}` : ""}`
                  : undefined
              }
            >
              <div className="row">
                <input type="text" style={{ flex: 1 }} placeholder={`${modelsDir}\\${p.hfFile}`} value={p.ggufPath} onChange={(e) => up({ ggufPath: e.target.value })} />
                <button onClick={browse}>Browse…</button>
              </div>
            </Field>
            <Field label="Hugging Face repo">
              <div className="row">
                <input type="text" style={{ flex: 1 }} value={p.hfRepo} onChange={(e) => up({ hfRepo: e.target.value })} />
                <button disabled={!p.hfRepo} onClick={() => setPicker(true)}>
                  Download…
                </button>
              </div>
            </Field>
            <Field label="File in repo" hint={p.hfSize ? `${fmtBytes(p.hfSize)} · SHA256 ${p.hfSha256 ? p.hfSha256.slice(0, 16) + "…" : "unknown"}` : undefined}>
              <input type="text" value={p.hfFile} onChange={(e) => up({ hfFile: e.target.value, hfSize: 0, hfSha256: "" })} />
            </Field>
            <Field label="Launch arguments" hint="Extra llama-server flags. Relay adds -m, --host, --port, -c (from the context limit) and -np 1.">
              <input type="text" className="mono" value={p.launchArgs} onChange={(e) => up({ launchArgs: e.target.value })} />
            </Field>
          </>
        ) : (
          <>
            <Field label="Base URL" hint="OpenAI-compatible, including /v1 — LM Studio http://localhost:1234/v1 · Ollama http://localhost:11434/v1 · llama-server http://127.0.0.1:8080/v1">
              <input type="text" value={p.external.baseUrl} onChange={(e) => up({ external: { ...p.external, baseUrl: e.target.value } })} />
            </Field>
            <Field label="Model name" hint="Sent as `model` (required by LM Studio/Ollama; ignored by llama-server).">
              <input type="text" value={p.external.model} onChange={(e) => up({ external: { ...p.external, model: e.target.value } })} />
            </Field>
            <Field label="API key">
              <div className="row">
                <label className="row">
                  <input type="checkbox" checked={p.external.useApiKey} onChange={(e) => up({ external: { ...p.external, useApiKey: e.target.checked } })} /> Send a key
                </label>
                {p.external.useApiKey && (
                  <>
                    <input type="password" placeholder="stored in Credential Manager" value={epKey} onChange={(e) => setEpKey(e.target.value)} />
                    <button
                      onClick={async () => {
                        await api.setEndpointKey(p.id, epKey);
                        setEpKey("");
                        toast("Endpoint key saved", "ok");
                      }}
                    >
                      Save key
                    </button>
                  </>
                )}
              </div>
            </Field>
          </>
        )}
        <Field label="Context limit (tokens)" hint="Used for budgeting and passed to llama-server as -c.">
          <input type="number" value={p.contextLimit} onChange={(e) => up({ contextLimit: num(e.target.value) })} />
        </Field>
        <Field label="Max output tokens">
          <input type="number" value={p.maxOutputTokens} onChange={(e) => up({ maxOutputTokens: num(e.target.value) })} />
        </Field>
        <Field label="Sampling">
          <div className="row wrap">
            {(
              [
                ["temperature", "temp"],
                ["topP", "top_p"],
                ["topK", "top_k"],
                ["minP", "min_p"],
                ["presencePenalty", "presence"],
                ["repeatPenalty", "repeat"],
              ] as const
            ).map(([k, label]) => (
              <label key={k} className="row" style={{ gap: 4 }}>
                <span className="faint">{label}</span>
                <input type="number" step="0.05" style={{ width: 70 }} value={p.sampling[k]} onChange={(e) => up({ sampling: { ...p.sampling, [k]: num(e.target.value) } })} />
              </label>
            ))}
          </div>
        </Field>
        <Field label="Planner sampling" hint="Used when this profile plans or reviews (thinking on). Off: the coder sampling above.">
          <div className="row wrap">
            <label className="row" style={{ gap: 4 }}>
              <input
                type="checkbox"
                checked={!!p.thinkingSampling}
                onChange={(e) => up({ thinkingSampling: e.target.checked ? { ...p.sampling, temperature: 0.6, topP: 0.95 } : null })}
              />
              <span className="faint">own values</span>
            </label>
            {p.thinkingSampling &&
              (
                [
                  ["temperature", "temp"],
                  ["topP", "top_p"],
                  ["topK", "top_k"],
                  ["minP", "min_p"],
                  ["presencePenalty", "presence"],
                ] as const
              ).map(([k, label]) => (
                <label key={k} className="row" style={{ gap: 4 }}>
                  <span className="faint">{label}</span>
                  <input
                    type="number"
                    step="0.05"
                    style={{ width: 70 }}
                    value={p.thinkingSampling![k]}
                    onChange={(e) => up({ thinkingSampling: { ...p.thinkingSampling!, [k]: num(e.target.value) } })}
                  />
                </label>
              ))}
          </div>
        </Field>
        <Field
          label="Reasoning"
          hint={
            p.reasoning.mode === "disable_thinking"
              ? 'Sends chat_template_kwargs: {"enable_thinking": false} (Qwen3.x).'
              : p.reasoning.mode === "effort"
                ? "Sends reasoning_effort (top level and as a chat-template kwarg); thinking can't be fully disabled, so it is stripped."
                : "Sends nothing reasoning-related."
          }
        >
          <div className="row wrap">
            <select value={p.reasoning.mode} onChange={(e) => up({ reasoning: { ...p.reasoning, mode: e.target.value as Profile["reasoning"]["mode"] } })}>
              <option value="disable_thinking">Disable thinking (enable_thinking=false)</option>
              <option value="effort">Reasoning effort</option>
              <option value="none">None</option>
            </select>
            {p.reasoning.mode === "effort" && (
              <select value={p.reasoning.effort} onChange={(e) => up({ reasoning: { ...p.reasoning, effort: e.target.value } })}>
                {["low", "medium", "high"].map((x) => (
                  <option key={x}>{x}</option>
                ))}
              </select>
            )}
            <label className="row">
              <input type="checkbox" checked={p.reasoning.stripThinkTags} onChange={(e) => up({ reasoning: { ...p.reasoning, stripThinkTags: e.target.checked } })} /> strip &lt;think&gt;
            </label>
            <label className="row">
              <input type="checkbox" checked={p.reasoning.stripHarmony} onChange={(e) => up({ reasoning: { ...p.reasoning, stripHarmony: e.target.checked } })} /> strip harmony
            </label>
            <label className="row" style={{ gap: 4 }}>
              <span className="faint">reserve</span>
              <input type="number" style={{ width: 80 }} value={p.reasoning.reasoningReserve} onChange={(e) => up({ reasoning: { ...p.reasoning, reasoningReserve: num(e.target.value) } })} />
            </label>
          </div>
        </Field>
      </div>
      <div className="row">
        <button onClick={test} disabled={testing}>
          {testing ? "Loading & testing…" : "Test profile"}
        </button>
        <span className="faint" style={{ fontSize: 12 }}>
          loads the model (unloading the current one) and runs a tiny edit task
        </span>
      </div>
      {testRes &&
        (typeof testRes === "string" ? (
          <div className="note err">{testRes}</div>
        ) : (
          <div className={`note ${testRes.ok && testRes.formatOk ? "ok" : "err"}`}>
            {testRes.message}
            {testRes.loadSecs ? ` Loaded in ${testRes.loadSecs.toFixed(1)}s.` : ""}
            <pre className="mono" style={{ margin: "6px 0 0", whiteSpace: "pre-wrap", color: "var(--text)" }}>
              {testRes.output}
            </pre>
          </div>
        ))}
      {picker && <DownloadPicker profile={p} onClose={() => setPicker(false)} />}
    </div>
  );
}

function Models() {
  const profiles = useStore((s) => s.profiles);
  const [draft, setDraft] = useState<Profile[]>(profiles);
  const [sel, setSel] = useState(profiles[0]?.id ?? "");
  useEffect(() => setDraft(profiles), [profiles]);
  const dirty = JSON.stringify(draft) !== JSON.stringify(profiles);
  const cur = draft.find((p) => p.id === sel) ?? draft[0];
  const replace = (p: Profile) => setDraft((d) => d.map((x) => (x.id === p.id ? p : x)));
  const add = (base?: Profile) => {
    const id = `custom-${Math.random().toString(36).slice(2, 8)}`;
    const p: Profile = base
      ? { ...structuredClone(base), id, builtin: false, name: `${base.name} (copy)` }
      : {
          ...structuredClone(draft[0]),
          id,
          builtin: false,
          name: "New profile",
          role: "",
          ggufPath: "",
          hfRepo: "",
          hfFile: "",
          hfSize: 0,
          hfSha256: "",
          launchArgs: "-ngl all -fa on",
        };
    setDraft((d) => [...d, p]);
    setSel(id);
  };
  return (
    <div className="col" style={{ gap: 10, minHeight: 0 }}>
      <div className="row">
        <span className="muted">Profiles are saved to {useStore.getState().boot?.appDir}\profiles.json.</span>
        <span style={{ flex: 1 }} />
        {dirty && (
          <>
            <button onClick={() => setDraft(profiles)}>Revert</button>
            <button className="primary" onClick={() => saveProfiles(draft)}>
              Save profiles
            </button>
          </>
        )}
      </div>
      <div style={{ display: "grid", gridTemplateColumns: "220px 1fr", gap: 14 }}>
        <div className="col">
          <div className="profile-list">
            {draft.map((p) => (
              <div key={p.id} className={`profile-card ${p.id === cur?.id ? "sel" : ""}`} onClick={() => setSel(p.id)}>
                <div className="col" style={{ gap: 0, flex: 1, minWidth: 0 }}>
                  <b style={{ overflow: "hidden", textOverflow: "ellipsis" }}>{p.name}</b>
                  <span className="faint" style={{ fontSize: 11.5 }}>
                    {p.builtin ? "built-in" : "custom"} · {p.kind} · {Math.round(p.contextLimit / 1024)}k
                  </span>
                </div>
              </div>
            ))}
          </div>
          <div className="row wrap">
            <button className="small" onClick={() => add()}>
              Add
            </button>
            <button className="small" disabled={!cur} onClick={() => cur && add(cur)}>
              Duplicate
            </button>
            {cur && !cur.builtin && (
              <button className="small danger" onClick={() => setDraft((d) => d.filter((x) => x.id !== cur.id))}>
                Delete
              </button>
            )}
            {cur?.builtin && (
              <button
                className="small"
                onClick={async () => {
                  try {
                    const def = await api.resetProfile(cur.id);
                    replace(def);
                    set({ profiles: (await api.getAppState()).profiles });
                    toast(`${def.name} reset to defaults`, "ok");
                  } catch (e) {
                    toast(errText(e), "error");
                  }
                }}
              >
                Reset to default
              </button>
            )}
          </div>
          {cur && (
            <button className="small" disabled={dirty} title={dirty ? "Save profiles first" : ""} onClick={() => switchProfile(cur.id)}>
              Load this model now
            </button>
          )}
        </div>
        {cur && (
          <div>
            {dirty && <div className="note info" style={{ marginBottom: 8 }}>Unsaved changes — save profiles before testing or loading.</div>}
            <ProfileEditor key={cur.id} p={cur} onChange={replace} />
          </div>
        )}
      </div>
    </div>
  );
}

function Templates() {
  const dark = useDarkMode();
  const [list, setList] = useState<TemplateInfo[]>([]);
  const [sel, setSel] = useState("expand.md");
  const [text, setText] = useState("");
  useEffect(() => {
    api.templatesList().then((l) => {
      setList(l);
      setText(l.find((t) => t.name === sel)?.text ?? "");
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const cur = list.find((t) => t.name === sel);
  const desc: Record<string, string> = {
    "expand.md":
      "Turns your instruction into the spec (planner: local model or Claude). Placeholders: {{instruction}} {{model_name}} {{context_limit}} {{project_name}} {{file_tree}} {{context_files}} {{project_outline}} {{explore_note}} {{budget_guidance}} {{verification_defaults}}",
    "implement-system.md": "System prompt for the coder: the SEARCH/REPLACE edit format rules.",
    "review.md": "Review prompt (planner). Placeholders: {{spec}} {{diff}} {{files_after}} {{verification}}",
    "fix.md": "The coder's fix round after a failed build / test / run. Placeholders: {{files}} {{spec}} {{failure}}",
  };
  return (
    <div className="col" style={{ gap: 8, flex: 1, minHeight: 0, height: "100%" }}>
      <div className="row">
        <select
          value={sel}
          onChange={(e) => {
            setSel(e.target.value);
            setText(list.find((t) => t.name === e.target.value)?.text ?? "");
          }}
        >
          {list.map((t) => (
            <option key={t.name} value={t.name}>
              {t.name}
              {t.isDefault ? "" : " (edited)"}
            </option>
          ))}
        </select>
        <span style={{ flex: 1 }} />
        <button className="small" onClick={() => cur && api.openInExplorer(cur.path)}>
          Show file
        </button>
        <button
          className="small"
          onClick={async () => {
            const t = await api.templateReset(sel);
            setText(t);
            setList(await api.templatesList());
            toast("Template reset to default", "ok");
          }}
        >
          Reset to default
        </button>
        <button
          className="small primary"
          disabled={text === cur?.text}
          onClick={async () => {
            await api.templateSave(sel, text);
            setList(await api.templatesList());
            toast("Template saved", "ok");
          }}
        >
          Save
        </button>
      </div>
      <span className="faint" style={{ fontSize: 12 }}>
        {desc[sel]}
      </span>
      <div className="editor-box" style={{ minHeight: 380 }}>
        <Editor language="markdown" theme={dark ? "vs-dark" : "vs"} value={text} onChange={(v) => setText(v ?? "")} options={{ wordWrap: "on", minimap: { enabled: false }, fontSize: 12.5, automaticLayout: true }} />
      </div>
    </div>
  );
}

export function SettingsDialog() {
  const open = useStore((s) => s.settingsOpen);
  const settings = useStore((s) => s.settings);
  const [tab, setTab] = useState<Tab>("general");
  const version = useStore((s) => s.boot?.version);
  if (!open || !settings) return null;
  const tabs: [Tab, string][] = [
    ["general", "General"],
    ["roles", "Roles & loop"],
    ["claude", "Claude"],
    ["models", "Local models"],
    ["templates", "Prompt templates"],
  ];
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && set({ settingsOpen: false })}>
      <div className="dialog settings" role="dialog" aria-label="Settings">
        <div className="dhead">
          <h3>Settings</h3>
          <span className="faint mono" style={{ fontSize: 11 }} title="Version · commit and date of this build">
            Relay {version}
          </span>
          <button className="small ghost" onClick={() => set({ settingsOpen: false })}>
            ✕
          </button>
        </div>
        <div className="settings-grid">
          <nav className="settings-nav">
            {tabs.map(([id, label]) => (
              <button key={id} className={tab === id ? "active" : ""} onClick={() => setTab(id)}>
                {label}
              </button>
            ))}
          </nav>
          <div className="settings-main">
            {tab === "general" && <General />}
            {tab === "roles" && <Roles />}
            {tab === "claude" && <Claude />}
            {tab === "models" && <Models />}
            {tab === "templates" && <Templates />}
          </div>
        </div>
      </div>
    </div>
  );
}
