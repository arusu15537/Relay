import { useEffect, useRef } from "react";
import { set, useStore } from "../store";

export function LogPanel() {
  const open = useStore((s) => s.logOpen);
  const log = useStore((s) => s.log);
  const server = useStore((s) => s.server);
  const ref = useRef<HTMLPreElement>(null);
  const stick = useRef(true);
  useEffect(() => {
    const el = ref.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [log, open]);
  return (
    <div className="logpanel">
      <div className="head" onClick={() => set({ logOpen: !open })}>
        <span>{open ? "▾" : "▸"}</span>
        <span>llama-server log</span>
        <span className="faint">
          {log.length} lines{server?.pid ? ` · pid ${server.pid}` : ""}
        </span>
        <span style={{ flex: 1 }} />
        {open && (
          <button
            className="small ghost"
            onClick={(e) => {
              e.stopPropagation();
              navigator.clipboard.writeText(log.join("\n"));
            }}
          >
            Copy
          </button>
        )}
      </div>
      {open && (
        <pre
          ref={ref}
          onScroll={(e) => {
            const el = e.currentTarget;
            stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 30;
          }}
        >
          {log.slice(-1500).map((l, i) => (
            <div key={i} className={`logline ${l.startsWith("[relay]") ? "relay" : / E |error|failed/i.test(l) && !l.includes("unused tensor") ? "err" : ""}`}>
              {l}
            </div>
          ))}
        </pre>
      )}
    </div>
  );
}
