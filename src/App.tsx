import { useEffect } from "react";
import { boot } from "./actions";
import { errText } from "./api";
import { Modal, Toasts } from "./components/common";
import { HistoryDrawer } from "./components/HistoryDrawer";
import { InstructionPane } from "./components/InstructionPane";
import { LogPanel } from "./components/LogPanel";
import { ResultPane } from "./components/ResultPane";
import { SettingsDialog } from "./components/Settings";
import { SpecPane } from "./components/SpecPane";
import { Toolbar } from "./components/Toolbar";
import { toast, useStore } from "./store";

let booted = false;

export default function App() {
  const ready = useStore((s) => s.ready);
  useEffect(() => {
    if (booted) return; // StrictMode double-invokes effects in dev
    booted = true;
    boot().catch((e) => toast(`Startup failed: ${errText(e)}`, "error"));
  }, []);
  if (!ready) {
    return (
      <div className="app">
        <div className="empty">Starting Relay…</div>
        <Toasts />
      </div>
    );
  }
  return (
    <div className="app">
      <Toolbar />
      <main className="panes">
        <InstructionPane />
        <SpecPane />
        <ResultPane />
      </main>
      <LogPanel />
      <SettingsDialog />
      <HistoryDrawer />
      <Modal />
      <Toasts />
    </div>
  );
}
