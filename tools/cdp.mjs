// Drives the dev app's WebView2 over the Chrome DevTools Protocol (no mouse needed).
// Start the app with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222, then:
//   node tools/cdp.mjs eval "<js expression>"
//   node tools/cdp.mjs shot out.png
//   node tools/cdp.mjs call Input.dispatchKeyEvent '{"type":"keyDown","code":"F14"}'
const [cmd, arg] = process.argv.slice(2);
const targets = await (await fetch("http://127.0.0.1:9222/json")).json();
const page = targets.find((t) => t.type === "page");
if (!page) throw new Error("no page target");
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise((r) => ws.addEventListener("open", r, { once: true }));
let id = 0;
const call = (method, params = {}) =>
  new Promise((resolve, reject) => {
    const my = ++id;
    const onMsg = (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id !== my) return;
      ws.removeEventListener("message", onMsg);
      m.error ? reject(new Error(m.error.message)) : resolve(m.result);
    };
    ws.addEventListener("message", onMsg);
    ws.send(JSON.stringify({ id: my, method, params }));
  });

if (cmd === "eval") {
  const r = await call("Runtime.evaluate", { expression: arg, awaitPromise: true, returnByValue: true });
  console.log(JSON.stringify(r.result.value ?? r.exceptionDetails?.text ?? r.result.description));
} else if (cmd === "call") {
  console.log(JSON.stringify(await call(arg, JSON.parse(process.argv[4] ?? "{}"))));
} else if (cmd === "shot") {
  const r = await call("Page.captureScreenshot", { format: "png" });
  const { writeFileSync } = await import("node:fs");
  writeFileSync(arg, Buffer.from(r.data, "base64"));
  console.log("saved", arg);
}
ws.close();
