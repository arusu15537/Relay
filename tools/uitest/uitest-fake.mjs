// UI scenario against the fake llama-server: spec → implement → diff → accept → switch → re-run → compare.
import fs from "node:fs";
import { connect } from "./cdplib.mjs";

const OUT = process.argv[2] || ".";
const src = fs.readFileSync(new URL("../../src-tauri/examples/e2e.rs", import.meta.url), "utf8");
const SPEC = src.match(/pub const RETRY_SPEC: &str = r#"([\s\S]*?)"#;/)[1];

const c = await connect();
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (ok, what) => {
  results.push([ok, what]);
  log(ok ? "PASS" : "FAIL", what);
};
try {
  await c.ev("location.reload()").catch(() => {});
  await new Promise((r) => setTimeout(r, 1500));
  const c2 = await connect();
  Object.assign(c, c2);
  await c.wait("!!window.__relay && window.__relay.useStore.getState().ready", 20000, "app ready");
  check(true, "app booted, project restored from settings: " + (await c.state("s.root")));

  // Spec typed by hand (no Claude): use the actions hook (same as typing in Monaco).
  await c.ev(`window.__relay.actions.setSpec(${JSON.stringify(SPEC)})`);
  await c.wait("document.querySelectorAll('.chip').length >= 2", 10000, "spec chips");
  const chips = await c.ev("[...document.querySelectorAll('.chip')].map(c => c.textContent)");
  check(chips.some((t) => t.includes("src/http_client.py")) && chips.some((t) => t.includes("src/retry.py")), `spec parsed into file chips: ${JSON.stringify(chips)}`);
  check(await c.ev("!!document.querySelector('.budget')"), "budget panel shown");
  await c.shot(`${OUT}/ui-1-spec.png`);

  // Implement with Qwen3.8 (fake): model loads first.
  await c.click("Implement with");
  await c.wait("window.__relay.useStore.getState().server?.state === 'loading'", 5000, "model loading");
  await c.shot(`${OUT}/ui-2-loading.png`);
  await c.wait("(window.__relay.useStore.getState().runs[0]?.stream || '').length > 40", 20000, "streaming");
  await c.shot(`${OUT}/ui-3-streaming.png`);
  await c.wait("window.__relay.useStore.getState().runs[0]?.status !== 'running'", 60000, "run finished");
  const run = await c.state("s.runs[0]");
  check(run.status === "done" && run.response.result.validationOk, `run done & valid (tok/s ${run.response.result.avgTokPerSec?.toFixed(1)})`);
  await c.wait("!!document.querySelector('.diff-box .monaco-diff-editor')", 10000, "diff editor");
  await new Promise((r) => setTimeout(r, 800));
  await c.shot(`${OUT}/ui-4-diff.png`);
  // Model calls list each attempt's prompt tokens and how many came from the server's prompt cache.
  await c.ev("document.querySelector('details.calls')?.setAttribute('open', '')");
  await new Promise((r) => setTimeout(r, 300));
  const callsText = await c.ev("document.querySelector('details.calls')?.textContent || ''");
  check(/prompt \d+ \(\d+ from cache\)/.test(callsText), `model calls show prompt-cache reuse: "${callsText.match(/prompt \d+[^·]*/)?.[0]}"`);
  await c.shot(`${OUT}/ui-4b-calls.png`);
  await c.ev("document.querySelector('details.calls')?.removeAttribute('open')");

  // Accept both files (repo is clean → no modal).
  await c.click("Accept all");
  await c.wait("Object.values(window.__relay.useStore.getState().runs[0].decisions).every(d => d === 'accepted')", 10000, "all accepted");
  const disk = fs.readFileSync(String(await c.state("s.root")) + "/src/retry.py", "utf8");
  check(disk.includes("fake-llama-server"), "accepted files written to the project folder");
  await new Promise((r) => setTimeout(r, 600));
  check((await c.state("s.git?.dirty")) === true, "git status refreshed: now dirty");

  // Re-run with gpt-oss via footer → switches model, then compare.
  await c.select("select[aria-label='Re-run with model']", "gpt-oss-20b");
  await c.click("Re-run with");
  await c.wait("window.__relay.useStore.getState().runs.length === 2 && window.__relay.useStore.getState().runs[1].status !== 'running'", 60000, "second run done");
  const r2 = await c.state("s.runs[1]");
  check(r2.profileId === "gpt-oss-20b" && r2.response?.result.validationOk, "re-run with gpt-oss-20b produced a valid result");
  check((await c.state("s.server.profileId")) === "gpt-oss-20b", "toolbar switched to gpt-oss-20b");
  // Second run: dirty tree now → accepting would ask; instead compare.
  await c.select("select[aria-label='Compare with run']", run.runId);
  await c.wait("!!document.querySelector('.compare-grid')", 5000, "compare view");
  await new Promise((r) => setTimeout(r, 800));
  await c.shot(`${OUT}/ui-5-compare.png`);

  // Dirty-tree gate: try to accept a file of run 2 → modal with checkpoint option.
  await c.select("select[aria-label='Compare with run']", "");
  await new Promise((r) => setTimeout(r, 300));
  await c.click("Accept", ".file-list");
  await c.wait("!!window.__relay.useStore.getState().modal", 5000, "git modal");
  const modal = await c.state("s.modal.title");
  check(modal.includes("Uncommitted"), `dirty-tree warning shown: "${modal}"`);
  await c.shot(`${OUT}/ui-6-gitmodal.png`);
  await c.click("Create checkpoint commit");
  await c.wait("!window.__relay.useStore.getState().modal", 5000);
  await c.wait("Object.values(window.__relay.useStore.getState().runs[1].decisions).some(d => d === 'accepted')", 10000, "accepted after checkpoint");
  check(true, "checkpoint commit created and file accepted");

  // Toolbar switcher → Qwen3.6, log panel.
  await c.select("select[aria-label='Local model']", "qwen3.6-35b-a3b");
  await c.wait("window.__relay.useStore.getState().server?.state === 'ready' && window.__relay.useStore.getState().server?.profileId === 'qwen3.6-35b-a3b'", 30000, "switched to qwen3.6");
  check(true, "toolbar switch to Qwen3.6-35B-A3B → ready");
  await c.click("Log", ".toolbar");
  await new Promise((r) => setTimeout(r, 500));
  await c.shot(`${OUT}/ui-7-log.png`);
  await c.click("Log", ".toolbar");

  // History drawer.
  await c.click("History", ".toolbar");
  await c.wait("document.querySelectorAll('.history-item').length > 0", 5000, "history items");
  await c.shot(`${OUT}/ui-8-history.png`);
  await c.click("✕", ".drawer");

  // Settings → models tab.
  await c.click("Settings", ".toolbar");
  await c.click("Local models", ".settings-nav");
  await new Promise((r) => setTimeout(r, 500));
  await c.shot(`${OUT}/ui-9-settings.png`);
  await c.click("✕", ".settings");
} catch (e) {
  check(false, String(e));
  await c.shot(`${OUT}/ui-error.png`).catch(() => {});
} finally {
  const failed = results.filter((r) => !r[0]);
  log(`${results.length - failed.length} passed, ${failed.length} failed`);
  c.close();
  process.exit(failed.length ? 1 : 0);
}
