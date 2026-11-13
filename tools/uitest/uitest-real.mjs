// Full pipeline on real models through the UI: Claude spec → Qwen3.8 → re-run Qwen3.6 + gpt-oss → compare → accept.
import { connect } from "./cdplib.mjs";

const OUT = process.argv[2] || ".";
let c = await connect();
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (ok, what) => {
  results.push([ok, what]);
  log(ok ? "PASS" : "FAIL", what);
};
const S = "window.__relay.useStore.getState()";
const waitRun = async (n, label) => {
  await c.wait(`${S}.runs.length === ${n} && ${S}.runs[${n - 1}].status !== 'running'`, 900000, label);
  const r = await c.state(`(() => { const r = s.runs[${n - 1}]; const res = r.response?.result; return { model: r.profileName, ok: res?.validationOk, tps: res?.avgTokPerSec, ms: res?.durationMs, files: res?.files.map(f => f.path), attempts: res?.calls.map(c => c.attempts.length), err: r.error, mode: res?.mode, fails: res?.calls.flatMap(c => c.attempts.flatMap(a => a.outcomes.filter(o => !o.ok).map(o => o.path + ': ' + o.message))) }; })()`);
  log(JSON.stringify(r));
  return r;
};
try {
  await c.wait(`!!window.__relay && ${S}.ready && !!${S}.root`, 30000, "app ready");
  await c.type("textarea.instruction", "add a retry with backoff to the HTTP client");
  await c.ev(`(() => { const row = [...document.querySelectorAll('.tree-row')].find(r => r.textContent.includes('http_client.py')); row.querySelector('input').click(); return true; })()`);
  const t0 = Date.now();
  await c.click("Expand with Claude");
  await c.wait(`${S}.expandStatus !== 'running' && ${S}.expandStatus !== 'idle'`, 600000, "expansion");
  check((await c.state("s.expandStatus")) === "done", `Claude spec in ${((Date.now() - t0) / 1000).toFixed(0)}s: ${await c.state("s.expandInfo")}`);
  await c.wait(`!!${S}.prepared`, 10000);
  log("spec files:", JSON.stringify(await c.state("s.prepared.spec.files.map(f => f.path + ' (' + f.action + ')')")), "budget:", await c.state("s.prepared.budget.recommended"));

  // Qwen3.8-27B (default)
  await c.click("Implement with");
  await c.wait(`${S}.server?.state === 'loading'`, 10000, "loading").catch(() => {});
  await c.shot(`${OUT}/real-1-loading.png`);
  await c.wait(`(${S}.runs[0]?.stream || '').length > 300`, 120000, "streaming");
  await c.shot(`${OUT}/real-2-streaming.png`);
  const r1 = await waitRun(1, "qwen3.8 run");
  check(r1.ok, `Qwen3.8-27B: valid=${r1.ok} · ${r1.tps?.toFixed(1)} tok/s · ${(r1.ms / 1000).toFixed(1)}s · attempts ${JSON.stringify(r1.attempts)}`);
  await new Promise((r) => setTimeout(r, 1000));
  await c.shot(`${OUT}/real-3-qwen38-diff.png`);

  // Re-run with Qwen3.6
  await c.select("select[aria-label='Re-run with model']", "qwen3.6-35b-a3b");
  await c.click("Re-run with");
  const r2 = await waitRun(2, "qwen3.6 run");
  check(r2.ok, `Qwen3.6-35B-A3B: valid=${r2.ok} · ${r2.tps?.toFixed(1)} tok/s · ${(r2.ms / 1000).toFixed(1)}s · attempts ${JSON.stringify(r2.attempts)}`);

  // Re-run with gpt-oss
  await c.select("select[aria-label='Re-run with model']", "gpt-oss-20b");
  await c.click("Re-run with");
  const r3 = await waitRun(3, "gpt-oss run");
  check(r3.ok, `gpt-oss-20b: valid=${r3.ok} · ${r3.tps?.toFixed(1)} tok/s · ${(r3.ms / 1000).toFixed(1)}s · attempts ${JSON.stringify(r3.attempts)}`);

  // Compare gpt-oss (active) with Qwen3.8 run.
  const run1 = await c.state("s.runs[0].runId");
  await c.select("select[aria-label='Compare with run']", run1);
  await c.wait("!!document.querySelector('.compare-grid')", 5000);
  await new Promise((r) => setTimeout(r, 1200));
  await c.shot(`${OUT}/real-4-compare.png`);
  await c.select("select[aria-label='Compare with run']", "");

  // Accept the Qwen3.8 result.
  await c.ev(`window.__relay.useStore.setState({ activeRun: ${JSON.stringify(run1)} })`);
  await new Promise((r) => setTimeout(r, 500));
  if (r1.ok) {
    await c.click("Accept all");
    await c.wait(`Object.values(${S}.runs[0].decisions).every(d => d === 'accepted')`, 20000, "accepted");
    check(true, "Qwen3.8 result accepted and written");
  }
  await c.shot(`${OUT}/real-5-accepted.png`);
  await c.click("Unload", ".toolbar");
  await c.wait(`${S}.server?.state === 'unloaded'`, 20000, "unloaded");
  check(true, "model unloaded");
} catch (e) {
  check(false, String(e));
  await c.shot(`${OUT}/real-error.png`).catch(() => {});
} finally {
  const failed = results.filter((r) => !r[0]);
  log(`${results.length - failed.length} passed, ${failed.length} failed`);
  c.close();
  process.exit(failed.length ? 1 : 0);
}
