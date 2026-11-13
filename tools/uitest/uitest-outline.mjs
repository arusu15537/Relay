// UI scenario for context outlines (no model runs): a large context file shows an "outline" chip,
// clicking it sends the file in full, and the Settings switch turns outlines off.
// Needs a project containing src/models.py (the e2e project: %TEMP%\relay-e2e\project).
import fs from "node:fs";
import { connect } from "./cdplib.mjs";

const OUT = process.argv[2] || ".";
const src = fs.readFileSync(new URL("../../src-tauri/examples/e2e.rs", import.meta.url), "utf8");
const SPEC = src.match(/pub const RETRY_SPEC: &str = r#"([\s\S]*?)"#;/)[1];
const CTX = "src/models.py";

const c = await connect();
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const results = [];
const check = (ok, what) => {
  results.push([ok, what]);
  log(ok ? "PASS" : "FAIL", what);
};
const chip = `[...document.querySelectorAll('.chip')].find(c => c.textContent.includes(${JSON.stringify(CTX)}))`;
const file = `window.__relay.useStore.getState().prepared?.files.find(f => f.path === ${JSON.stringify(CTX)})`;
try {
  await c.wait("!!window.__relay && window.__relay.useStore.getState().ready", 20000, "app ready");
  check(!!(await c.state("s.root")), "project restored: " + (await c.state("s.root")));
  await c.ev(`window.__relay.actions.saveSettings({ outlineContext: true })`);
  await c.ev(`window.__relay.actions.toggleChecked([${JSON.stringify(CTX)}], true)`);
  await c.ev(`window.__relay.actions.setSpec(${JSON.stringify(SPEC)})`);
  await c.wait(`!!(${file})`, 10000, "prepared with the context file");

  // 1. Outline by default.
  await c.wait(`(${chip})?.textContent.includes('outline')`, 5000, "outline tag");
  const f1 = await c.ev(file);
  check(f1.outline && f1.tokens < f1.fullTokens, `context file sent as outline (≈${f1.tokens} of ≈${f1.fullTokens} tokens)`);
  check(await c.ev(`(${chip})?.tagName === 'BUTTON' && (${chip}).getAttribute('aria-pressed') === 'false'`), "chip is a toggle button, not pressed");
  check(
    (await c.ev(`[...document.querySelectorAll('.chip')].filter(c => c.tagName === 'BUTTON').length`)) === 1,
    "targets are plain chips (only the context file toggles)",
  );
  const total1 = await c.state("s.prepared.budget.single.inputTokens");
  await c.shot(`${OUT}/outline-1-chip.png`);

  // 2. Click → whole file.
  await c.ev(`(${chip}).click()`);
  await c.wait(`${file} && !${file}.outline`, 5000, "sent in full after click");
  const f2 = await c.ev(file);
  check(f2.tokens === f2.fullTokens && !(await c.ev(`(${chip}).textContent.includes('outline')`)), "click sends the whole file");
  check(JSON.stringify(await c.state("s.fullContext")) === JSON.stringify([CTX]), "fullContext remembers the choice");
  const total2 = await c.state("s.prepared.budget.single.inputTokens");
  check(total2 - total1 === f1.fullTokens - f1.tokens, `budget grows by the difference (${total1} → ${total2})`);
  check(await c.ev(`(${chip}).getAttribute('aria-pressed') === 'true'`), "chip shows pressed");
  await c.shot(`${OUT}/outline-2-full.png`);

  // 3. Click again → outline.
  await c.ev(`(${chip}).click()`);
  await c.wait(`${file}?.outline`, 5000, "outline again");
  check(true, "second click restores the outline");

  // 4. Settings switch off → no outlines, chip not clickable; and the checkbox is in Settings.
  await c.ev(`window.__relay.useStore.setState({ settingsOpen: true })`);
  await c.wait("[...document.querySelectorAll('label')].some(l => l.textContent.includes('Send large context files as outlines'))", 5000, "settings field");
  await c.shot(`${OUT}/outline-3-settings.png`);
  await c.ev(
    "[...document.querySelectorAll('label')].find(l => l.textContent.includes('Send large context files as outlines')).querySelector('input').click()",
  );
  await c.wait(`${file} && !${file}.outline`, 5000, "outlines off");
  check((await c.state("s.settings.outlineContext")) === false, "setting saved as off");
  await c.ev(`window.__relay.useStore.setState({ settingsOpen: false })`);
  await new Promise((r) => setTimeout(r, 300));
  check(await c.ev(`(${chip})?.tagName === 'SPAN'`), "with outlines off the chip is not a toggle");
  const saved = JSON.parse(fs.readFileSync(process.env.RELAY_APP_DIR + "/settings.json", "utf8"));
  check(saved.outlineContext === false, "settings.json has outlineContext: false");

  // Restore.
  await c.ev(`window.__relay.actions.saveSettings({ outlineContext: true })`);
} catch (e) {
  check(false, String(e));
} finally {
  const failed = results.filter(([ok]) => !ok).length;
  log(`${results.length - failed} passed, ${failed} failed`);
  c.close();
  process.exit(failed ? 1 : 0);
}
