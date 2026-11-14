// UI scenario: exact token counts and prompt prewarm while the spec is reviewed, against the fake
// llama-server in --fake-recurrent mode (profile `fake-recurrent`, see README). No GPU.
import { execFileSync } from "node:child_process";
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
const S = "window.__relay.useStore.getState()";
try {
  await c.ev("location.reload()").catch(() => {});
  await new Promise((r) => setTimeout(r, 1500));
  Object.assign(c, await connect());
  await c.wait(`!!window.__relay && ${S}.ready`, 20000, "app ready");
  // Start from nothing loaded (other scenarios may leave a model running) and from the seeded
  // project (other scenarios accept edits into it; the fake model's edits then become ambiguous).
  const root = await c.state("s.root");
  const first = execFileSync("git", ["-C", root, "rev-list", "--max-parents=0", "HEAD"]).toString().trim();
  execFileSync("git", ["-C", root, "reset", "-q", "--hard", first]);
  execFileSync("git", ["-C", root, "clean", "-fdq"]);
  await c.ev("window.__relay.actions.refreshFiles()");
  await c.ev("window.__relay.actions.stopServer()");
  await c.wait(`${S}.server?.state !== 'ready' && ${S}.server?.state !== 'loading'`, 15000, "no model loaded");

  // Prewarm is off by default; turn it on (same as the Settings checkbox).
  check((await c.state("s.settings.prewarmPrompt")) === false, "prewarm is off by default");
  await c.ev("window.__relay.actions.saveSettings({ prewarmPrompt: true })");

  // The spec before any model is loaded: estimated counts, no prewarm (it never loads a model).
  await c.ev(`window.__relay.actions.setSpec(${JSON.stringify(SPEC)})`);
  await c.wait(`${S}.prepared?.spec.files.length >= 2`, 10000, "spec prepared");
  check((await c.state("s.prepared.budget.exact")) === false, "budget estimated while no model is loaded");
  await new Promise((r) => setTimeout(r, 2500));
  check((await c.state("s.server?.state")) !== "loading" && (await c.state("s.prewarm")) === null, "no model loaded for a prewarm");

  // Load the recurrent fake from the toolbar: the budget turns exact and the prefix is prewarmed.
  await c.select("select[aria-label='Local model']", "fake-recurrent");
  await c.wait(`${S}.server?.state === 'ready' && ${S}.server?.profileId === 'fake-recurrent'`, 30000, "model ready");
  await c.wait(`${S}.prepared?.budget.exact === true`, 10000, "exact budget");
  const budgetText = await c.ev("document.querySelector('.budget')?.textContent || ''");
  check(budgetText.includes("exact"), `budget shows exact counts: ${budgetText.slice(0, 120)}`);
  await c.wait(`${S}.prewarm?.status === 'primed'`, 15000, "prewarm primed");
  const pw = await c.state("s.prewarm");
  const note = await c.ev("document.querySelector('.prewarm-note')?.textContent || ''");
  check(note.includes("prompt tokens ready"), `prewarm note shown: ${note}`);
  await c.shot(`${OUT}/prewarm-1-ready.png`);

  // Editing the spec keeps the prefix: no new prewarm work.
  await c.ev(`window.__relay.actions.setSpec(${JSON.stringify(SPEC + "\n- Keep the public API unchanged.\n")})`);
  await new Promise((r) => setTimeout(r, 2500));
  check((await c.state("s.prewarm?.status")) === "primed", "spec edit: prefix still warm (no re-prime)");

  // Implement: the first call reuses the whole primed prefix.
  await c.click("Implement with");
  await c.wait(`${S}.runs[0]?.status && ${S}.runs[0].status !== 'running'`, 60000, "run finished");
  const run = await c.state("s.runs[0]");
  const a = run.response?.result.calls[0]?.attempts[0];
  const primed = pw.outcomes[pw.outcomes.length - 1].tokens;
  check(
    run.response?.result.validationOk && a?.cachedTokens >= primed,
    `run valid (${run.response?.result.validationOk}, ${run.error ?? run.response?.result.error ?? "no error"}, profile ${run.profileId}); call 1 reused ${a?.cachedTokens} of ${primed} prewarmed tokens`,
  );
  check((await c.state("s.prewarm")) === null, "prewarm note cleared by the run");
  await c.shot(`${OUT}/prewarm-2-run.png`);

} catch (e) {
  check(false, `exception: ${e.message}`);
}
try {
  // Leave the app as the other scenarios expect it: prewarm off, nothing loaded, Qwen3.8 selected.
  await c.ev("window.__relay.actions.saveSettings({ prewarmPrompt: false, activeProfile: 'qwen3.8-27b' })");
  await c.ev("window.__relay.actions.stopServer()");
  await c.wait(`${S}.server?.state !== 'ready'`, 15000, "model unloaded");
} catch (e) {
  check(false, `exception: ${e.message}`);
} finally {
  c.close();
}
const failed = results.filter(([ok]) => !ok);
console.log(`\n${results.length - failed.length} passed, ${failed.length} failed`);
process.exit(failed.length ? 1 : 0);
