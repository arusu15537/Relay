import React from "react";
import ReactDOM from "react-dom/client";
import { loader } from "@monaco-editor/react";
// Lean Monaco: core editor API + editor features + Monarch syntax highlighting only. Language
// services (TS/JSON/CSS/HTML workers) are deliberately left out: diffs of partial projects would
// otherwise show misleading error squiggles, and they add ~9 MB of workers.
import * as monaco from "monaco-editor/editor";
import "monaco-editor/features/register.all";
import "monaco-editor/basic-languages/monaco.contribution";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
import App from "./App";
import * as actions from "./actions";
import { useStore } from "./store";
import "./styles.css";

// Bundle Monaco locally (the app must work offline). The base editor worker computes diffs.
self.MonacoEnvironment = {
  getWorker: () => new EditorWorker(),
};
loader.config({ monaco });

// Dev builds only: expose the store and actions for automated UI tests over DevTools.
if (import.meta.env.DEV) {
  (window as unknown as Record<string, unknown>).__relay = { useStore, actions };
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
