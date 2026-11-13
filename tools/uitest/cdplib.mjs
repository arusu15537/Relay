import fs from "node:fs";

const PORT = process.env.CDP_PORT || 9223;

export async function connect() {
  let page;
  for (let i = 0; i < 60 && !page; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json`)).json();
      page = list.find((t) => t.type === "page" && !t.url.startsWith("devtools"));
    } catch {}
    if (!page) await new Promise((r) => setTimeout(r, 500));
  }
  if (!page) throw new Error("no CDP target");
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => ((ws.onopen = res), (ws.onerror = rej)));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  };
  const send = (method, params = {}) =>
    new Promise((res) => {
      const i = ++id;
      pending.set(i, res);
      ws.send(JSON.stringify({ id: i, method, params }));
    });
  const ev = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    if (r.result?.exceptionDetails) throw new Error(`eval failed: ${expr.slice(0, 120)} → ${JSON.stringify(r.result.exceptionDetails.exception?.description ?? r.result.exceptionDetails).slice(0, 600)}`);
    return r.result?.result?.value;
  };
  const wait = async (pred, timeout = 30000, label = pred) => {
    const t0 = Date.now();
    while (Date.now() - t0 < timeout) {
      if (await ev(pred)) return Date.now() - t0;
      await new Promise((r) => setTimeout(r, 250));
    }
    throw new Error(`timeout waiting for: ${label}`);
  };
  const shot = async (file, w = 1560, h = 940) => {
    await send("Emulation.setDeviceMetricsOverride", { width: w, height: h, deviceScaleFactor: 1, mobile: false });
    await new Promise((r) => setTimeout(r, 500));
    const r = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(file, Buffer.from(r.result.data, "base64"));
    await send("Emulation.clearDeviceMetricsOverride");
  };
  /** Click the first button whose text includes `text` (optionally inside `scope` selector). */
  const click = async (text, scope = "body") => {
    const ok = await ev(`(() => { const b = [...document.querySelectorAll(${JSON.stringify(scope)} + ' button')].find(b => b.textContent.includes(${JSON.stringify(text)}) && !b.disabled); if (!b) return false; b.click(); return true; })()`);
    if (!ok) throw new Error(`button not found/enabled: ${text}`);
  };
  const type = async (selector, value) => {
    await ev(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); const proto = el.tagName === 'TEXTAREA' ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype; Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, ${JSON.stringify(value)}); el.dispatchEvent(new Event('input', { bubbles: true })); return true; })()`);
  };
  const select = async (selector, value) => {
    await ev(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(el, ${JSON.stringify(value)}); el.dispatchEvent(new Event('change', { bubbles: true })); return el.value; })()`);
  };
  const state = (path) => ev(`(() => { const s = window.__relay.useStore.getState(); return ${path}; })()`);
  return { ws, send, ev, wait, shot, click, type, select, state, close: () => ws.close() };
}
