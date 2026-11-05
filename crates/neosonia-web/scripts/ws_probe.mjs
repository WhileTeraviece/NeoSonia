// WebSocket 探针（真机验证用，不属于产品代码）。
// 用法：
//   node ws_probe.mjs --url "ws://127.0.0.1:8799/ws?token=..." --frames 3 --timeout 8000
//   可选 --append "conv_id=probe-long&since_seq=240" 追加 query（验证重连 resume）。
// 行为：连接后打印每个收到的帧（JSON 单行），收到第 2 帧时发 ping，第 3 帧时发 cancel。
// 退出码：0 = 成功收到 hello 且至少一帧；1 = 失败/超时。

const args = process.argv.slice(2);
function arg(name, fallback) {
  const i = args.indexOf(name);
  if (i >= 0 && i + 1 < args.length) return args[i + 1];
  const pref = args.find((a) => a.startsWith(name + '='));
  if (pref) return pref.slice(name.length + 1);
  return fallback;
}

let url = arg('--url', '');
const append = arg('--append', '');
if (append) url = url + '&' + append;
const framesWanted = Number(arg('--frames', '3'));
const timeoutMs = Number(arg('--timeout', '8000'));

if (!url) {
  console.error('用法: node ws_probe.mjs --url "ws://127.0.0.1:PORT/ws?token=..."');
  process.exit(1);
}

let received = 0;
let gotHello = false;
let pingSent = false;
let cancelSent = false;
let finished = false;

const timer = setTimeout(() => {
  console.error(`TIMEOUT: ${timeoutMs}ms 内只收到 ${received} 帧`);
  finish(1);
}, timeoutMs);

function finish(code) {
  if (finished) return;
  finished = true;
  clearTimeout(timer);
  try { ws.close(); } catch (e) {}
  console.log(`WS_RESULT frames=${received} hello=${gotHello} code=${code}`);
  setTimeout(() => process.exit(code), 60);
}

const ws = new WebSocket(url);

ws.addEventListener('open', () => {
  console.log('WS_OPEN');
});

ws.addEventListener('error', (ev) => {
  console.error('WS_ERROR', ev && ev.message ? ev.message : '');
});

ws.addEventListener('close', () => {
  if (!finished) finish(gotHello ? 0 : 1);
});

ws.addEventListener('message', (ev) => {
  received += 1;
  const text = typeof ev.data === 'string' ? ev.data : String(ev.data);
  console.log('FRAME ' + text.replace(/\r?\n/g, ' '));
  let parsed = null;
  try { parsed = JSON.parse(text); } catch (e) {}
  if (parsed && parsed.type === 'hello') {
    gotHello = true;
    if (parsed.data && parsed.data.resume) {
      console.log('RESUME ' + JSON.stringify(parsed.data.resume));
    }
  }
  if (received === 2 && !pingSent) {
    pingSent = true;
    ws.send(JSON.stringify({ type: 'ping' }));
  }
  if (received === 3 && !cancelSent) {
    cancelSent = true;
    ws.send(JSON.stringify({ type: 'cancel' }));
  }
  if (received >= framesWanted) finish(gotHello ? 0 : 1);
});
