// SPA 纯逻辑单测（Node 运行，不碰 DOM / 不需要 cargo）。
// 用法：node scripts/test-spa-core.cjs
// 覆盖：重连退避、心跳判定、静默重试、配对失败文案分类、配对码格式化/校验。

const assert = require('node:assert');
const core = require('../assets/web/spa-core.js');

let passed = 0;
const failures = [];

function test(name, fn) {
  try {
    fn();
    passed += 1;
    console.log('ok   ' + name);
  } catch (err) {
    failures.push(name + ': ' + (err && err.message ? err.message : err));
    console.log('FAIL ' + name + ' -> ' + (err && err.message ? err.message : err));
  }
}

test('重连退避序列 1s/2s/5s/10s/15s 封顶', () => {
  assert.deepStrictEqual(core.RECONNECT_STEPS, [1000, 2000, 5000, 10000, 15000]);
  assert.strictEqual(core.nextReconnectDelay(1), 1000);
  assert.strictEqual(core.nextReconnectDelay(2), 2000);
  assert.strictEqual(core.nextReconnectDelay(3), 5000);
  assert.strictEqual(core.nextReconnectDelay(4), 10000);
  assert.strictEqual(core.nextReconnectDelay(5), 15000);
  assert.strictEqual(core.nextReconnectDelay(6), 15000);
  assert.strictEqual(core.nextReconnectDelay(0), 1000);
  assert.strictEqual(core.nextReconnectDelay(99), 15000);
});

test('连续 3 次无心跳才判定断开', () => {
  assert.strictEqual(core.MAX_MISSED_PONGS, 3);
  assert.strictEqual(core.shouldDeclareDisconnected(0), false);
  assert.strictEqual(core.shouldDeclareDisconnected(1), false);
  assert.strictEqual(core.shouldDeclareDisconnected(2), false);
  assert.strictEqual(core.shouldDeclareDisconnected(3), true);
  assert.strictEqual(core.shouldDeclareDisconnected(4), true);
});

test('短暂断线先静默重试 2 次，不弹断开提示', () => {
  assert.strictEqual(core.SILENT_RETRIES, 2);
  assert.strictEqual(core.shouldShowDisconnectBanner(0), false);
  assert.strictEqual(core.shouldShowDisconnectBanner(1), false);
  assert.strictEqual(core.shouldShowDisconnectBanner(2), false);
  assert.strictEqual(core.shouldShowDisconnectBanner(3), true);
});

test('心跳间隔为 25 秒', () => {
  assert.strictEqual(core.HEARTBEAT_INTERVAL_MS, 25000);
});

test('配对码格式化/校验', () => {
  assert.strictEqual(core.formatPairCode('a2c4e6f8'), 'A2C4-E6F8');
  assert.strictEqual(core.formatPairCode(' A2C4 E6F8 '), 'A2C4-E6F8');
  assert.strictEqual(core.formatPairCode('A2C4E6F8'), 'A2C4-E6F8');
  assert.strictEqual(core.formatPairCode('AB'), 'AB');
  assert.strictEqual(core.isValidPairCode('A2C4E6F8'), true);
  assert.strictEqual(core.isValidPairCode('A2C4-E6F8'), true);
  assert.strictEqual(core.isValidPairCode('A2C4'), false);
  assert.strictEqual(core.isValidPairCode(''), false);
  assert.strictEqual(core.normalizePairCode('a2c4-e6f8'), 'A2C4E6F8');
});

test('配对失败：网络不通', () => {
  const msg = core.pairFailureMessage(0, '');
  assert.ok(msg.includes('无法连接桌面端'), msg);
  assert.ok(msg.includes('同一 Wi-Fi'), msg);
});

test('配对失败：码已被使用', () => {
  const msg = core.pairFailureMessage(401, '配对码已被使用过（一次性）。请在桌面端刷新配对码后重试。');
  assert.ok(msg.includes('已被使用'), msg);
  assert.ok(msg.includes('刷新配对码'), msg);
});

test('配对失败：码已过期', () => {
  const msg = core.pairFailureMessage(401, '配对码已过期。请在桌面端刷新配对码后重试。');
  assert.ok(msg.includes('已过期'), msg);
  assert.ok(msg.includes('刷新配对码'), msg);
});

test('配对失败：码不正确', () => {
  const msg = core.pairFailureMessage(401, '配对码不正确，请核对后重试。');
  assert.ok(msg.includes('不正确'), msg);
});

test('配对失败：不是同一局域网', () => {
  const msg = core.pairFailureMessage(403, '只接受局域网内的设备连接');
  assert.ok(msg.includes('不是同一局域网'), msg);
  assert.ok(msg.includes('同一个 Wi-Fi'), msg);
});

test('配对失败：设备数上限', () => {
  const msg = core.pairFailureMessage(409, '已信任设备数达到上限');
  assert.ok(msg.includes('上限'), msg);
  assert.ok(msg.includes('撤销'), msg);
});

test('配对失败：服务异常', () => {
  const msg = core.pairFailureMessage(500, '');
  assert.ok(msg.includes('服务异常'), msg);
});

test('配对失败：401 未知原因回退到服务端文案', () => {
  const msg = core.pairFailureMessage(401, '未授权：请先用配对码配对这台设备。');
  assert.strictEqual(msg, '未授权：请先用配对码配对这台设备。');
});

test('API 状态文案：绿色也显示 detail（最近事件）', () => {
  const text = core.providerStatusText({
    provider: 'DeepSeek',
    indicator: 'green',
    label: '正常',
    detail: 'DeepSeek 网页/API 部分中断（已恢复）'
  });
  assert.strictEqual(text, 'DeepSeek · 正常（DeepSeek 网页/API 部分中断（已恢复））');
});

test('API 状态文案：无 detail 时只显示 provider · label', () => {
  const text = core.providerStatusText({ provider: 'DeepSeek', indicator: 'yellow', label: '轻微故障' });
  assert.strictEqual(text, 'DeepSeek · 轻微故障');
});

test('API 状态文案：unknown/null 返回空串（小球隐藏）', () => {
  assert.strictEqual(core.providerStatusText(null), '');
  assert.strictEqual(core.providerStatusText(undefined), '');
  assert.strictEqual(core.providerStatusText({ indicator: 'unknown', label: '未知' }), '');
  assert.strictEqual(core.providerStatusText({}), '');
});

test('宽屏判定：桌面/平板横屏为宽屏，手机竖屏保持单栏', () => {
  // 桌面 1440x900 → 宽屏；手机 430x900 → 单栏。
  assert.strictEqual(core.isWideLayout(1440, 900), true);
  assert.strictEqual(core.isWideLayout(1024, 768), true);
  // >=768 的横屏（平板横屏）算宽屏。
  assert.strictEqual(core.isWideLayout(800, 500), true);
  // >=900 无论方向都算宽屏（避免竖屏平板太窄时硬塞两栏？900 以上是明确宽屏）。
  assert.strictEqual(core.isWideLayout(900, 1200), true);
  // 手机竖屏/小平板竖屏 → 单栏。
  assert.strictEqual(core.isWideLayout(430, 900), false);
  assert.strictEqual(core.isWideLayout(768, 1024), false);
  assert.strictEqual(core.isWideLayout(360, 740), false);
  // 非数字输入安全。
  assert.strictEqual(core.isWideLayout(undefined, undefined), false);
});

// ============================================================
// v0.9.2：多 token 本地存储（迁移/去重/上限/只移除失败项/配对追加/断开仅当前）
// ============================================================

test('token 列表：规范化（丢弃非法/去重保留第一个/最多 8 个）', () => {
  assert.strictEqual(core.MAX_TOKENS, 8);
  const raw = [
    { token: 'a', device: { id: 'd1' }, savedAt: 1 },
    { token: ' a ', device: { id: 'dup' }, savedAt: 2 }, // 去重：保留第一个
    { token: '', device: null },
    { token: 123 },
    null,
    { token: 'b', savedAt: '3' },
    { token: 'c', device: 'not-object' }
  ];
  const list = core.normalizeTokenList(raw);
  assert.strictEqual(list.length, 3);
  assert.deepStrictEqual(list[0], { token: 'a', device: { id: 'd1' }, savedAt: 1 });
  assert.strictEqual(list[1].token, 'b');
  assert.strictEqual(list[1].savedAt, 3);
  assert.strictEqual(list[2].device, null);

  const many = [];
  for (let i = 0; i < 20; i++) many.push({ token: 't' + i, savedAt: i });
  assert.strictEqual(core.normalizeTokenList(many).length, 8);
  assert.deepStrictEqual(core.normalizeTokenList(null), []);
  assert.deepStrictEqual(core.normalizeTokenList('not-array'), []);
});

test('token 列表：旧单 token 迁移', () => {
  const list = core.migrateLegacyToken('legacy-token', { id: 'old-device' }, 42);
  assert.deepStrictEqual(list, [{ token: 'legacy-token', device: { id: 'old-device' }, savedAt: 42 }]);
  assert.deepStrictEqual(core.migrateLegacyToken('', { id: 'x' }, 1), []);
});

test('token 列表：配对追加（unshift/去重/最多 8 个，不动其它项）', () => {
  const base = [{ token: 'old1', savedAt: 1 }, { token: 'old2', savedAt: 2 }];
  const added = core.addToken(base, 'new', { id: 'new-device' }, 100);
  assert.deepStrictEqual(added.map((it) => it.token), ['new', 'old1', 'old2']);
  assert.deepStrictEqual(base.map((it) => it.token), ['old1', 'old2'], '不应修改原数组');

  // 已存在：移动到最前，不产生重复。
  const moved = core.addToken(added, 'old2', null, 200);
  assert.deepStrictEqual(moved.map((it) => it.token), ['old2', 'new', 'old1']);
  assert.strictEqual(moved.length, 3);

  // 上限：只保留最新 8 个。
  let list = [];
  for (let i = 0; i < 10; i++) list = core.addToken(list, 'k' + i, null, i);
  assert.strictEqual(list.length, 8);
  assert.strictEqual(list[0].token, 'k9');

  assert.deepStrictEqual(core.addToken(base, '', null, 1).map((it) => it.token), ['old1', 'old2']);
});

test('token 列表：失败/断开时只移除指定项，其它项保留', () => {
  const list = [
    { token: 'bad', savedAt: 1 },
    { token: 'good', savedAt: 2 },
    { token: 'other', savedAt: 3 }
  ];
  const after = core.removeToken(list, 'bad');
  assert.deepStrictEqual(after.map((it) => it.token), ['good', 'other']);
  assert.strictEqual(list.length, 3, '不应修改原数组');
  // 未知/空 token 不误删。
  assert.deepStrictEqual(core.removeToken(after, 'missing').map((it) => it.token), ['good', 'other']);
  assert.deepStrictEqual(core.removeToken(after, '').map((it) => it.token), ['good', 'other']);
});

test('token 列表：更新设备信息不改变顺序、不清空', () => {
  const list = [{ token: 'a', device: { id: '1' }, savedAt: 1 }, { token: 'b', device: { id: '2' }, savedAt: 2 }];
  const next = core.updateTokenDevice(list, 'a', { id: '1-new', name: '我的设备' });
  assert.deepStrictEqual(next.map((it) => it.token), ['a', 'b']);
  assert.deepStrictEqual(next[0].device, { id: '1-new', name: '我的设备' });
  assert.deepStrictEqual(next[1].device, { id: '2' });
});

if (failures.length) {
  console.error('\n' + failures.length + ' 个失败：');
  failures.forEach((f) => console.error('  - ' + f));
  process.exit(1);
}
console.log('\nSPA_CORE_TESTS_PASS ' + passed);

