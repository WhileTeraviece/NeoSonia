// SPA 离线渲染自检用的 mock 服务（不是产品代码，不参与编译）。
// 用途：在 cargo 编译窗口之外，用真实 assets/web 里的 SPA 验证 v0.4 UI
//（骨架屏/分页/审查条/模式 chip/设备页性能计时）的 JS 运行时正确性。
// 用法：node scripts/mock-server.mjs [port]
// 说明：只实现 GET 读接口 + POST /api/pair|chat|new；不实现 WebSocket（连不上属预期，顶栏会显示未连接）。

import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const webDir = path.resolve(__dirname, '../assets/web');
const iconPath = path.resolve(__dirname, '../../../assets/icon-64.png');
const port = Number(process.argv[2] || 8899);

const now = Date.now();
const device = {
  id: 'mock-device-1',
  name: 'mock 手机',
  user_agent: 'mock',
  last_addr: '127.0.0.1',
  created_at_ms: now - 60000,
  last_seen_ms: now,
  online: true
};

const sessions = [
  {
    id: 'probe-long',
    title: '长会话分页测试',
    created_at_ms: now - 7200000,
    updated_at_ms: now - 60000,
    message_count: 240,
    preview: '分页测试回复 240：seq 窗口应连续且不重叠。',
    persona: 'default'
  },
  {
    id: 'probe-conv-1',
    title: '探针测试会话',
    created_at_ms: now - 3600000,
    updated_at_ms: now - 120000,
    message_count: 2,
    preview: '探针：这是历史里的助手回复，用于验证会话读取。',
    persona: 'default'
  }
];

function messagesFor(convId) {
  const total = convId === 'probe-long' ? 240 : 2;
  const out = [];
  for (let i = 1; i <= total; i++) {
    const user = i % 2 === 1;
    out.push({
      id: 'm' + i,
      seq: i,
      role: user ? 'user' : 'assistant',
      content: user
        ? '分页测试消息 ' + i + '：用于验证远端加载更早消息。'
        : '分页测试回复 ' + i + '：seq 窗口应连续且不重叠。',
      created_at_ms: now - (total - i) * 1000,
      has_image: false,
      image_count: 0
    });
  }
  return out;
}

const reviews = [
  {
    call_id: 'probe-review-1',
    tool: 'run_command',
    args_summary: 'pwsh -NoProfile -Command "Get-ChildItem C:\\Windows\\System32"',
    risk_level: 'high',
    risk_reason: '执行系统命令并读取系统目录',
    class: 'shell',
    emphasized: true
  },
  {
    call_id: 'probe-review-2',
    tool: 'write_file',
    args_summary: 'config/settings.toml（覆盖写入）',
    risk_level: 'medium',
    risk_reason: '会修改桌面端配置文件',
    class: 'fs',
    emphasized: false
  }
];

function readAsset(name) {
  return fs.readFileSync(path.join(webDir, name), 'utf8');
}

function pageHtml() {
  const icon = fs.readFileSync(iconPath);
  const uri = 'data:image/png;base64,' + icon.toString('base64');
  return readAsset('index.html').replace('{{FAVICON_DATA_URI}}', uri).replace(/\{\{APP_VERSION\}\}/g, 'mock');
}

function json(res, value, status = 200) {
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8' });
  res.end(JSON.stringify(value));
}

function send(res, status, type, body) {
  res.writeHead(status, { 'Content-Type': type });
  res.end(body);
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://127.0.0.1');
  const p = url.pathname;

  if (p === '/' || p === '/pair') return send(res, 200, 'text/html; charset=utf-8', pageHtml());
  if (p === '/app.js') return send(res, 200, 'application/javascript; charset=utf-8', readAsset('app.js'));
  if (p === '/style.css') return send(res, 200, 'text/css; charset=utf-8', readAsset('style.css'));
  if (p === '/spa-core.js') return send(res, 200, 'application/javascript; charset=utf-8', readAsset('spa-core.js'));
  if (p === '/healthz') return json(res, { ok: true });

  if (p === '/api/pair' && req.method === 'POST') {
    return json(res, { token: 'mock-token-0123456789', device });
  }
  if (p === '/api/me') {
    return json(res, {
      device,
      server: {
        hostname: 'MOCK-HOST',
        version: '0.1.0',
        busy: false,
        model: 'mock-model',
        mode: 'coding',
        mode_label: '编程',
        review_pending: reviews.length,
        lan_ip: '127.0.0.1',
        lan_url: 'http://127.0.0.1:' + port,
        pairing_active: true
      }
    });
  }
  if (p === '/api/devices') return json(res, [device]);
  if (p === '/api/review/pending') return json(res, reviews);
  if (p === '/api/sessions') return json(res, sessions);
  if (p.startsWith('/api/sessions/')) {
    const rest = p.slice('/api/sessions/'.length);
    if (rest.endsWith('/delete')) return json(res, { deleted: true });
    const id = decodeURIComponent(rest);
    const all = messagesFor(id);
    const limit = Number(url.searchParams.get('limit') || (url.searchParams.has('before') ? 100 : 200));
    const beforeParam = url.searchParams.get('before');
    let windowMsgs;
    if (beforeParam === null) {
      windowMsgs = all.slice(-limit);
    } else {
      const before = Number(beforeParam);
      windowMsgs = all.filter((m) => m.seq < before).slice(-limit);
    }
    return json(res, windowMsgs);
  }
  if (p === '/api/search') return json(res, []);
  if (p === '/api/chat' && req.method === 'POST') {
    req.resume();
    return json(res, { turn_id: 'mock-turn' }, 202);
  }
  if (p === '/api/new' && req.method === 'POST') return json(res, { ok: true });
  if (p.startsWith('/api/screenshot/')) return json(res, { error: 'mock 没有截图' }, 404);

  send(res, 404, 'application/json; charset=utf-8', JSON.stringify({ error: 'mock: not found' }));
});

server.listen(port, '127.0.0.1', () => {
  console.log('MOCK_READY http://127.0.0.1:' + port);
});

