// UI scenario for the self-reliant loop against the fake llama-server: the local planner writes
// the spec (with a tool call), the Spec pane shows the verification commands, the run builds in a
// staging copy, fails, gets fixed, is reviewed (NEEDS_FIXES → follow-up → PASS), then accepted.
// Finally autopilot: plan → implement → verify → review without stopping at the spec.
//
// Relay must run with FAKE_LLM_SCRIPT pointing at the same file as this script uses
// (default %TEMP%\relay-uitest\fake-script.json), so the fake server answers from these rules.
import fs from "node:fs";
import path from "node:path";
import { connect } from "./cdplib.mjs";

const OUT = process.argv[2] || ".";
const TMP = process.env.TEMP || "C:/Windows/Temp";
const SCRIPT = process.env.FAKE_LLM_SCRIPT || path.join(TMP, "relay-uitest", "fake-script.json");
const PROJECT = path.join(TMP, "relay-uitest", "loop-project");

const SPEC = `## Summary
Mark the app as fixed.

## Design
- \`app.txt\` holds one status line.

## Files
- \`app.txt\` (modify) — status line

## Changes
### \`app.txt\`
The status line must read \`status: FIXED\`.

## Edge cases
- None.

## Acceptance criteria
- \`app.txt\` contains \`FIXED\`.

## Verification
- build: \`findstr /C:"FIXED" app.txt\`
`;
const FOLLOW = SPEC.replace("Mark the app as fixed.", "Add a note line.").replace(
  "The status line must read `status: FIXED`.",
  "Add the line `follow-up done` after the status line.",
);
const RULES = [
  { contains: "# Instruction", after_tool: false, reasoning: "Let me read app.txt first.", tool_calls: [{ name: "read_file", arguments: { path: "app.txt" } }], response: "(unused)" },
  { contains: "# Instruction", after_tool: true, reasoning: "app.txt says broken; the spec must make it FIXED.", response: SPEC },
  { contains: "Add the line `follow-up done`", response: "app.txt\n<<<<<<< SEARCH\nstatus: FIXED\n=======\nstatus: FIXED\nfollow-up done\n>>>>>>> REPLACE\n" },
  { contains: "follow-up done", reasoning: "Both requirements hold.", response: "## Verdict\nPASS — the status and the note are right.\n\n## Problems\nNone." },
  { contains: "# Verification results", reasoning: "The note is missing.", response: `## Verdict\nNEEDS_FIXES — the note line is missing.\n\n## Problems\n- \`app.txt\`: no note.\n\n=== FOLLOW-UP SPEC ===\n${FOLLOW}` },
  { contains: "# What failed", response: "app.txt\n<<<<<<< SEARCH\nstatus: broken\n=======\nstatus: FIXED\n>>>>>>> REPLACE\n" },
];

function resetProject() {
  fs.rmSync(PROJECT, { recursive: true, force: true });
  fs.mkdirSync(PROJECT, { recursive: true });
  fs.writeFileSync(path.join(PROJECT, "app.txt"), "status: broken\n");
  fs.writeFileSync(path.join(PROJECT, "notes.md"), "# Notes\n");
}

const c = await connect();
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (ok, what) => {
  results.push([ok, what]);
  log(ok ? "PASS" : "FAIL", what);
};
let prevRoot = "";
const S = (expr) => c.ev(`(() => { const s = window.__relay.useStore.getState(); return ${expr}; })()`);
try {
  await c.ev("location.reload()").catch(() => {});
  await new Promise((r) => setTimeout(r, 1500));
  Object.assign(c, await connect());
  await c.wait("!!window.__relay && window.__relay.useStore.getState().ready", 20000, "app ready");
  prevRoot = await S("s.root");
  fs.writeFileSync(SCRIPT, JSON.stringify(RULES));
  resetProject();
  await c.ev(
    "window.__relay.actions.saveSettings({ plannerBackend: 'local', plannerProfile: 'qwen3.6-35b-a3b', coderProfile: 'qwen3.6-35b-a3b', plannerTools: true, verifyEnabled: true, loopReview: true, followUpRounds: 1, maxFixRounds: 3, autopilot: false })",
  );
  await c.ev("window.__relay.useStore.setState({ implementProfile: 'qwen3.6-35b-a3b' })");
  await c.ev(`window.__relay.actions.openProject(${JSON.stringify(PROJECT)})`);
  await c.wait(`window.__relay.useStore.getState().root === ${JSON.stringify(PROJECT)}`, 10000, "loop project open");

  // Plan with the local planner.
  await c.type("textarea.instruction", "mark the app as fixed");
  check(await c.ev("[...document.querySelectorAll('button')].some(b => b.textContent.includes('Plan with'))"), "Instruction pane offers 'Plan with' + the planner model");
  await c.click("Plan with");
  await c.wait("window.__relay.useStore.getState().expandStatus !== 'running'", 60000, "planner finished");
  const info = await S("s.expandInfo");
  check((await S("s.expandStatus")) === "done" && /local planner/.test(info) && /1 file look-up/.test(info), `spec written by the local planner: "${info}"`);
  check((await S("s.spec")).includes("## Verification"), "the spec has a Verification section");
  await c.wait("!!document.querySelector('.chip.ctx.mono')", 10000, "verification chips");
  const vchips = await c.ev("[...document.querySelectorAll('.chip.ctx.mono')].map(x => x.textContent)");
  check(vchips.some((t) => t.includes("findstr")), `Spec pane lists the verification command: ${JSON.stringify(vchips)}`);
  await c.shot(`${OUT}/loop-1-spec.png`);

  // Implement: build fails → fix → review → follow-up → PASS.
  await c.click("Implement with");
  await c.wait("(window.__relay.useStore.getState().runs[0]?.steps || []).some(x => x.kind === 'verify')", 60000, "first verify step");
  await c.shot(`${OUT}/loop-2-live.png`);
  await c.wait("window.__relay.useStore.getState().runs[0]?.status !== 'running'", 120000, "loop finished");
  const run = await S("s.runs[0]");
  const wf = run.response?.result.workflow;
  check(!!wf && /verified/.test(wf.outcome) && /review PASS/.test(wf.outcome), `loop outcome: "${wf?.outcome}"`);
  check(run.status === "done" && wf.steps.length === 9, `9 steps shown (${wf?.steps.map((x) => x.kind + (x.ok ? "+" : "-")).join(" ")})`);
  await c.wait("document.querySelectorAll('.note .steps li').length >= 9", 10000, "workflow panel");
  check(await c.ev("[...document.querySelectorAll('details.cmd summary')].some(x => x.textContent.includes('findstr'))"), "command outputs are listed under the steps");
  check(run.review?.passed === true, "the final review is shown as PASS");
  await new Promise((r) => setTimeout(r, 800));
  await c.shot(`${OUT}/loop-3-done.png`);
  check(fs.readFileSync(path.join(PROJECT, "app.txt"), "utf8") === "status: broken\n", "project untouched before accepting");
  await c.click("Accept all");
  await c.wait("Object.values(window.__relay.useStore.getState().runs[0].decisions).every(d => d === 'accepted')", 10000, "accepted");
  check(fs.readFileSync(path.join(PROJECT, "app.txt"), "utf8").startsWith("status: FIXED\nfollow-up done\n"), "accepted file written to the project");

  // Settings tab.
  await c.ev("window.__relay.useStore.setState({ settingsOpen: true })");
  await c.click("Roles & loop");
  await new Promise((r) => setTimeout(r, 400));
  check(await c.ev("!!document.querySelector('select[aria-label=\"Planner model\"]') && !!document.querySelector('select[aria-label=\"Coder model\"]')"), "Settings → Roles & loop shows planner and coder");
  await c.shot(`${OUT}/loop-4-settings.png`);
  await c.ev("window.__relay.useStore.setState({ settingsOpen: false })");

  // Autopilot: plan → implement without stopping at the spec.
  resetProject();
  await c.ev("window.__relay.actions.newJob()");
  await c.ev("window.__relay.actions.saveSettings({ autopilot: true })");
  await c.ev("window.__relay.actions.refreshFiles()");
  await c.type("textarea.instruction", "mark the app as fixed");
  await c.click("Plan with");
  await c.wait("window.__relay.useStore.getState().runs.length === 1", 60000, "autopilot started the run");
  await c.wait("window.__relay.useStore.getState().runs[0]?.status !== 'running'", 120000, "autopilot loop finished");
  const auto = await S("s.runs[0].response?.result.workflow?.outcome");
  check(/verified/.test(auto ?? ""), `autopilot ran the whole loop: "${auto}"`);
  await c.shot(`${OUT}/loop-5-autopilot.png`);
} catch (e) {
  check(false, String(e));
  await c.shot(`${OUT}/loop-error.png`).catch(() => {});
} finally {
  await c.ev("window.__relay.actions.saveSettings({ autopilot: false })").catch(() => {});
  // Leave the app on the project the other scenarios expect.
  if (prevRoot && prevRoot !== PROJECT) await c.ev(`window.__relay.actions.openProject(${JSON.stringify(prevRoot)})`).catch(() => {});
  fs.writeFileSync(SCRIPT, "[]");
}
const failed = results.filter(([ok]) => !ok);
console.log(`\n${results.length - failed.length}/${results.length} passed`);
process.exit(failed.length ? 1 : 0);
