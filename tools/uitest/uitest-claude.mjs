// UI scenario with the real Claude CLI: instruction → expand → spec → implement (fake model) → accept → review.
import { connect } from "./cdplib.mjs";

const OUT = process.argv[2] || ".";
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
  await c.wait(`!!window.__relay && ${S}.ready && !!${S}.root`, 20000, "app ready");
  await c.ev("window.__relay.actions.newJob()");
  await c.type("textarea.instruction", "add a retry with backoff to the HTTP client");
  // Tick src/http_client.py in the tree.
  await c.ev(`(() => { const row = [...document.querySelectorAll('.tree-row')].find(r => r.textContent.includes('http_client.py')); row.querySelector('input').click(); return true; })()`);
  await c.wait(`${S}.checked.includes('src/http_client.py')`, 3000, "file ticked");
  check(true, "instruction typed and src/http_client.py ticked as context");
  const t0 = Date.now();
  await c.ev("window.__relay.actions.saveSettings({ plannerBackend: 'claude', verifyEnabled: false, loopReview: false })");
  await c.click("Expand with Claude");
  await c.wait(`${S}.expandStatus === 'running'`, 5000);
  await c.wait(`${S}.claudeStream.length > 200`, 240000, "Claude streaming");
  await c.shot(`${OUT}/ui-c1-expanding.png`);
  await c.wait(`${S}.expandStatus !== 'running'`, 600000, "expansion finished");
  const st = await c.state("({status: s.expandStatus, err: s.expandError, info: s.expandInfo, files: s.prepared?.spec.files.map(f => f.path + ' (' + f.action + ')'), warnings: s.prepared?.spec.warnings, activity: s.claudeActivity})");
  check(st.status === "done", `expansion done in ${((Date.now() - t0) / 1000).toFixed(0)}s: ${st.info} ${st.err ?? ""}`);
  log("tools used by Claude:", JSON.stringify(st.activity));
  await c.wait(`!!${S}.prepared`, 10000, "prepared");
  const st2 = await c.state("({files: s.prepared.spec.files.map(f => f.path + ' (' + f.action + ')'), warnings: s.prepared.spec.warnings, budget: s.prepared.budget.recommended})");
  check(st2.files.length > 0, `spec lists files: ${JSON.stringify(st2.files)}`);
  check(st2.warnings.length === 0, `spec structure parsed without warnings ${JSON.stringify(st2.warnings)}`);
  await c.shot(`${OUT}/ui-c2-spec.png`);
  log("SPEC:\n" + (await c.state("s.spec")));

  // Implement with the fake model, accept, review with Claude.
  await c.click("Implement with");
  await c.wait(`${S}.runs.length === 1 && ${S}.runs[0].status !== 'running'`, 60000, "run done");
  check(await c.state("s.runs[0].response?.result.validationOk"), "fake model produced valid edits for Claude's spec");
  await c.click("Accept all");
  await c.wait(`Object.values(${S}.runs[0].decisions).every(d => d === 'accepted')`, 10000, "accepted");
  await c.click("Review applied diff with Claude");
  await c.wait(`${S}.runs[0].reviewStatus === 'running'`, 5000);
  await c.wait(`${S}.runs[0].reviewStatus !== 'running'`, 600000, "review finished");
  const rv = await c.state("s.runs[0].review");
  check(!!rv && !!rv.verdict, `review verdict: ${rv?.verdict} · follow-up spec: ${!!rv?.followUpSpec}`);
  await c.shot(`${OUT}/ui-c3-review.png`);
  if (rv?.followUpSpec) {
    await c.click("Use follow-up spec");
    await c.wait(`${S}.spec.length > 0 && ${S}.runs.length === 0`, 5000);
    check(true, "follow-up spec loaded into the Spec pane as a new job");
  }
} catch (e) {
  check(false, String(e));
  await c.shot(`${OUT}/ui-c-error.png`).catch(() => {});
} finally {
  const failed = results.filter((r) => !r[0]);
  log(`${results.length - failed.length} passed, ${failed.length} failed`);
  c.close();
  process.exit(failed.length ? 1 : 0);
}
