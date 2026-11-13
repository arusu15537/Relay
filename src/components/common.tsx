import { useEffect, useState } from "react";
import { useStore } from "../store";
import type { PaneStatus, ServerState } from "../types";

export function StatusDot({ status, label }: { status: PaneStatus | ServerState; label?: string }) {
  return (
    <span className="status" title={`Status: ${status}`}>
      <span className={`dot ${status}`} />
      {label ?? status}
    </span>
  );
}

export function useDarkMode(): boolean {
  const q = "(prefers-color-scheme: dark)";
  const [dark, setDark] = useState(() => window.matchMedia(q).matches);
  useEffect(() => {
    const m = window.matchMedia(q);
    const on = (e: MediaQueryListEvent) => setDark(e.matches);
    m.addEventListener("change", on);
    return () => m.removeEventListener("change", on);
  }, []);
  return dark;
}

const LANGS: Record<string, string> = {
  ts: "typescript",
  tsx: "typescript",
  mts: "typescript",
  cts: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  py: "python",
  rs: "rust",
  go: "go",
  cs: "csharp",
  java: "java",
  kt: "kotlin",
  c: "c",
  h: "c",
  cpp: "cpp",
  cc: "cpp",
  hpp: "cpp",
  json: "json",
  md: "markdown",
  html: "html",
  css: "css",
  scss: "scss",
  yml: "yaml",
  yaml: "yaml",
  toml: "ini",
  ini: "ini",
  sh: "shell",
  ps1: "powershell",
  sql: "sql",
  rb: "ruby",
  php: "php",
  swift: "swift",
  xml: "xml",
};

export function langFor(path: string): string {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  return LANGS[ext] ?? "plaintext";
}

export function Modal() {
  const modal = useStore((s) => s.modal);
  useEffect(() => {
    if (!modal) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") modal.resolve("cancel");
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [modal]);
  if (!modal) return null;
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && modal.resolve("cancel")}>
      <div className="dialog" style={{ width: 520 }} role="dialog" aria-label={modal.title}>
        <div className="dhead">
          <h3>{modal.title}</h3>
        </div>
        <div className="dbody">
          <div>{modal.body}</div>
          {modal.details && modal.details.length > 0 && <pre className="stream" style={{ maxHeight: 180, minHeight: 0 }}>{modal.details.join("\n")}</pre>}
        </div>
        <div className="dfoot">
          {modal.buttons.map((b) => (
            <button key={b.id} className={b.kind === "primary" ? "primary" : b.kind === "danger" ? "danger" : ""} onClick={() => modal.resolve(b.id)}>
              {b.label}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

export function Toasts() {
  const toasts = useStore((s) => s.toasts);
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: React.ReactNode }) {
  return (
    <>
      <label>{label}</label>
      <div>{children}</div>
      {hint && <div className="hint">{hint}</div>}
    </>
  );
}
