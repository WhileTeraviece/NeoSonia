// 用 Edge/CDP 截取局域网 SPA 的"已配对聊天页"：
// 1) 启动 headless Edge（--remote-debugging-port=9444）
// 2) 打开 SPA 配对页 → 走 /api/pair 拿 token → 写入 localStorage → 刷新
// 3) Page.captureScreenshot 存 PNG
//
// 用法: node scripts/spa-chat-shot.mjs <baseUrl> <pairCode> <outPng> [port]
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { writeFileSync } from "node:fs";
import process from "node:process";

const base = (process.argv[2] || "http://127.0.0.1:8787").replace(/\/$/, "");
const code = process.argv[3] || "";
const out = process.argv[4] || "E:/NeoSonia/logs/remote/spa-chat.png";
const port = Number(process.argv[5] || 9444);
const presetToken = process.argv[6] || "";

const edgePath = [
  "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
  "C:/Program Files/Microsoft/Edge/Application/msedge.exe",
].find((p) => existsSync(p));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function getJson(path) {
  const r = await fetch(`http://127.0.0.1:${port}${path}`);
  return r.json();
}

async function main() {
  if (!edgePath) {
    console.error("找不到 msedge.exe");
    process.exit(1);
  }
  const profile = `${process.env.TEMP}\\neo-edge-spa-${Date.now()}`;
  const child = spawn(
    edgePath,
    [
      "--headless=new",
      "--disable-gpu",
      "--no-first-run",
      `--remote-debugging-port=${port}`,
      `--user-data-dir=${profile}`,
      "--window-size=430,930",
      "about:blank",
    ],
    { stdio: "ignore", detached: true },
  );
  child.unref();

  // 等调试端口
  let target = null;
  for (let i = 0; i < 60; i++) {
    await sleep(300);
    try {
      const list = await getJson("/json/list");
      target = list.find((t) => t.type === "page");
      if (target) break;
    } catch {}
  }
  if (!target) {
    console.error("CDP 端口没起来");
    process.exit(2);
  }

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  let id = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  };
  const send = (method, params = {}) =>
    new Promise((resolve) => {
      const myId = ++id;
      pending.set(myId, resolve);
      ws.send(JSON.stringify({ id: myId, method, params }));
    });
  await new Promise((r) => (ws.onopen = r));

  await send("Page.enable");
  await send("Runtime.enable");
  await send("Page.navigate", { url: `${base}/pair#${code}` });
  await sleep(2500);

  // 有现成 token 就直接注入；否则在页面里完成配对
  const injectExpr = presetToken
    ? `(() => { localStorage.setItem('neosonia.token', ${JSON.stringify(presetToken)}); return 'token injected'; })()`
    : `(async () => {
    const r = await fetch('/api/pair', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ code: ${JSON.stringify(code)}, name: 'CDP 截图设备', user_agent: 'cdp-shot' })
    });
    if (!r.ok) return 'pair failed ' + r.status;
    const j = await r.json();
    localStorage.setItem('neosonia.token', j.token);
    localStorage.setItem('neosonia.device', JSON.stringify(j.device));
    return 'paired ' + j.device.name;
  })()`;
  const pairRes = await send("Runtime.evaluate", {
    expression: injectExpr,
    awaitPromise: true,
    returnByValue: true,
  });
  console.log("配对/注入:", JSON.stringify(pairRes.result?.result?.value));

  await send("Page.navigate", { url: `${base}/` });
  await sleep(2000);

  // 让聊天页发一条消息，截到真实对话
  const chatExpr = `(async () => {
    const t = localStorage.getItem('neosonia.token') || '';
    const r = await fetch('/api/chat', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'Authorization': 'Bearer ' + t },
      body: JSON.stringify({ text: '用一句话说明你是运行在我电脑上的助手。' })
    });
    return 'chat ' + r.status;
  })()`;
  const chatRes = await send("Runtime.evaluate", {
    expression: chatExpr,
    awaitPromise: true,
    returnByValue: true,
  });
  console.log("聊天提交:", JSON.stringify(chatRes.result?.result?.value));
  await sleep(6000);

  const shot = await send("Page.captureScreenshot", { format: "png" });
  const data = shot.result?.data;
  if (!data) {
    console.error("截图失败");
    process.exit(3);
  }
  writeFileSync(out, Buffer.from(data, "base64"));
  console.log("截图已保存:", out);
  try {
    ws.close();
  } catch {}
  process.exit(0);
}

main().catch((e) => {
  console.error("失败:", e);
  process.exit(1);
});
