#!/usr/bin/env node
// NeoSonia MCP 演示服务器：stdio（默认）或 HTTP（`--http [port]`）。
//
// 只依赖 Node 内置模块，用于真机验证 neosonia-mcp 客户端：
// - stdio 模式：stdin/stdout 上跑 newline-delimited JSON-RPC 2.0；
// - http 模式：POST JSON-RPC；initialize / tools/call 回 application/json，
//   tools/list 故意回 text/event-stream（验证客户端能解析 SSE）。
// - `--stall` 模式：只读 stdin、故意不回包（验证客户端启动握手超时）。

import http from "node:http";
import readline from "node:readline";

function log(...args) {
  process.stderr.write("[mcp-demo] " + args.join(" ") + "\n");
}

const tools = [
  {
    name: "echo",
    description: "回显传入的文本。",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string", description: "要回显的文本" } },
      required: ["text"],
    },
  },
  {
    name: "add",
    description: "两个数相加，返回 {\"sum\": 结果}。",
    inputSchema: {
      type: "object",
      properties: {
        a: { type: "number", description: "加数一" },
        b: { type: "number", description: "加数二" },
      },
      required: ["a", "b"],
    },
  },
  {
    name: "sleep",
    description: "等待指定毫秒后返回（用于验证客户端超时）。",
    inputSchema: {
      type: "object",
      properties: { ms: { type: "number", description: "等待毫秒数" } },
      required: ["ms"],
    },
  },
];

function initializeResult(params) {
  return {
    protocolVersion: params?.protocolVersion ?? "2024-11-05",
    capabilities: { tools: { listChanged: false } },
    serverInfo: { name: "neosonia-demo", version: "0.1.0" },
  };
}

async function callTool(params) {
  const name = params?.name;
  const args = params?.arguments ?? {};
  if (name === "echo") {
    return { content: [{ type: "text", text: String(args.text ?? "") }], isError: false };
  }
  if (name === "add") {
    const a = Number(args.a);
    const b = Number(args.b);
    if (!Number.isFinite(a) || !Number.isFinite(b)) {
      return { content: [{ type: "text", text: "参数 a、b 必须是数字。" }], isError: true };
    }
    return { content: [{ type: "text", text: JSON.stringify({ sum: a + b }) }], isError: false };
  }
  if (name === "sleep") {
    const ms = Math.max(0, Number(args.ms) || 0);
    await new Promise((resolve) => setTimeout(resolve, ms));
    return { content: [{ type: "text", text: `等待了 ${ms} 毫秒。` }], isError: false };
  }
  return { content: [{ type: "text", text: `未知工具：${name}` }], isError: true };
}

// ---------------- stdio 模式 ----------------

function startStdioServer() {
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  rl.on("line", async (line) => {
    const text = line.trim();
    if (!text) {
      return;
    }
    let message;
    try {
      message = JSON.parse(text);
    } catch {
      log("收到无法解析的行，已忽略：", text.slice(0, 120));
      return;
    }
    const { id, method, params } = message;
    const isNotification = id === undefined || id === null;
    log("收到", method, isNotification ? "（通知）" : `（id=${id}）`);
    if (isNotification) {
      return;
    }
    const send = (payload) => process.stdout.write(JSON.stringify(payload) + "\n");
    switch (method) {
      case "initialize":
        send({ jsonrpc: "2.0", id, result: initializeResult(params) });
        return;
      case "ping":
        send({ jsonrpc: "2.0", id, result: {} });
        return;
      case "tools/list":
        send({ jsonrpc: "2.0", id, result: { tools } });
        return;
      case "tools/call":
        send({ jsonrpc: "2.0", id, result: await callTool(params) });
        return;
      default:
        send({ jsonrpc: "2.0", id, error: { code: -32601, message: `不支持的方法：${method}` } });
    }
  });
  rl.on("close", () => {
    log("stdin 已关闭，退出。");
    process.exit(0);
  });
  log("stdio 演示服务器已就绪。");
}

// ---------------- HTTP 模式 ----------------

function startHttpServer(portArg) {
  const requestedPort = Number(portArg);
  const sessionId = "demo-session-" + Math.random().toString(36).slice(2, 10);
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => {
      body += chunk;
    });
    req.on("end", async () => {
      const headerSession = req.headers["mcp-session-id"];
      if (headerSession && headerSession !== sessionId) {
        log("警告：客户端带了不认识的 Mcp-Session-Id：", headerSession);
      }
      let message;
      try {
        message = JSON.parse(body || "{}");
      } catch {
        res.writeHead(400, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ error: "请求体不是 JSON" }));
        return;
      }
      const { id, method, params } = message;
      const isNotification = id === undefined || id === null;
      log("HTTP 收到", method, isNotification ? "（通知）" : `（id=${id}）`);
      const sendJson = (payload) => {
        res.writeHead(200, {
          "Content-Type": "application/json",
          "Mcp-Session-Id": sessionId,
        });
        res.end(JSON.stringify(payload));
      };
      const sendSse = (payload) => {
        res.writeHead(200, {
          "Content-Type": "text/event-stream",
          "Mcp-Session-Id": sessionId,
        });
        res.end(`event: message\ndata: ${JSON.stringify(payload)}\n\n`);
      };
      if (isNotification) {
        res.writeHead(202, { "Mcp-Session-Id": sessionId });
        res.end();
        return;
      }
      switch (method) {
        case "initialize":
          sendJson({ jsonrpc: "2.0", id, result: initializeResult(params) });
          return;
        case "ping":
          sendJson({ jsonrpc: "2.0", id, result: {} });
          return;
        case "tools/list":
          // 故意用 SSE 回：验证客户端能解析 text/event-stream
          sendSse({ jsonrpc: "2.0", id, result: { tools } });
          return;
        case "tools/call":
          sendJson({ jsonrpc: "2.0", id, result: await callTool(params) });
          return;
        default:
          sendJson({
            jsonrpc: "2.0",
            id,
            error: { code: -32601, message: `不支持的方法：${method}` },
          });
      }
    });
  });
  server.on("error", (error) => {
    log("HTTP 服务器错误：", error.message);
    process.exit(1);
  });
  const port = Number.isFinite(requestedPort) ? requestedPort : 0;
  server.listen(port, "127.0.0.1", () => {
    const actualPort = server.address().port;
    // 第一行 stdout 给测试脚本读，方便用 0 端口自动分配
    process.stdout.write(`NEOSONIA_MCP_HTTP_PORT=${actualPort}\n`);
    log(`HTTP 演示服务器已监听 http://127.0.0.1:${actualPort}/mcp`);
  });
}

// ---------------- 入口 ----------------

const httpIndex = process.argv.indexOf("--http");
if (httpIndex !== -1) {
  startHttpServer(process.argv[httpIndex + 1] ?? "0");
} else if (process.argv.includes("--stall")) {
  // 只读 stdin、故意不回包：验证客户端启动握手超时。
  const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  rl.on("line", (line) => log("--stall 模式收到行（故意不回包）：", line.slice(0, 80)));
  rl.on("close", () => process.exit(0));
  log("stall 演示服务器已就绪（不会回任何包）。");
} else {
  startStdioServer();
}
