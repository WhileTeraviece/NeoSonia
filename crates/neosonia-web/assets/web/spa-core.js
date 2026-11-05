/* NeoSonia 远程 SPA 的纯逻辑核心（可在 Node 里单测，不碰 DOM）。
 *
 * 被 app.js 通过 window.NeoSoniaCore 使用，也被 scripts/test-spa-core.cjs 单测。
 * 内容：重连退避 / 心跳判定 / 配对失败文案分类 / 配对码格式化。
 */
(function (root, factory) {
  var api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  if (root) root.NeoSoniaCore = api;
})(typeof window !== 'undefined' ? window : null, function () {
  'use strict';

  // 1s → 2s → 5s → 10s → 最大 15s
  var RECONNECT_STEPS = [1000, 2000, 5000, 10000, 15000];
  // 客户端心跳间隔 25s；连续 3 次没有收到任何帧才判定断开。
  var HEARTBEAT_INTERVAL_MS = 25000;
  var MAX_MISSED_PONGS = 3;
  // 断线后先静默重试 2 次，仍失败才显示"连接断开"提示，避免误报。
  var SILENT_RETRIES = 2;

  function nextReconnectDelay(attempt) {
    var n = Math.max(1, Math.floor(Number(attempt) || 1));
    var idx = Math.min(n - 1, RECONNECT_STEPS.length - 1);
    return RECONNECT_STEPS[idx];
  }

  function shouldDeclareDisconnected(missedPongs) {
    return Math.floor(Number(missedPongs) || 0) >= MAX_MISSED_PONGS;
  }

  function shouldShowDisconnectBanner(attempt) {
    return Math.floor(Number(attempt) || 0) > SILENT_RETRIES;
  }

  // 配对失败文案分类：码错误/已使用/已过期/不是同一局域网/设备数上限/网络不通/服务异常。
  function pairFailureMessage(status, serverError) {
    var msg = String(serverError || '').trim();
    var code = Number(status) || 0;
    if (code === 0) {
      return '无法连接桌面端：请确认手机和电脑连接同一 Wi-Fi，且桌面端「远程访问」已开启。';
    }
    if (code === 401) {
      if (msg.indexOf('已被使用') >= 0) return '配对码已被使用过：请在桌面端点「刷新配对码」后重新扫码。';
      if (msg.indexOf('过期') >= 0) return '配对码已过期：请在桌面端点「刷新配对码」后重新扫码。';
      if (msg.indexOf('不正确') >= 0) return '配对码不正确：请核对桌面端显示的 8 位配对码。';
      if (msg.indexOf('授权已失效') >= 0 || msg.indexOf('已撤销') >= 0) {
        return '设备授权已失效：请在桌面端查看新配对码。';
      }
      return msg || '配对未成功：请重新扫码或手动输入配对码。';
    }
    if (code === 403) {
      return '不是同一局域网：请确认手机和电脑连接同一个 Wi-Fi（关闭 VPN 后再试）。';
    }
    if (code === 409) {
      return '设备数已达上限：请在桌面端「远程访问」页撤销一台旧设备后重试。';
    }
    if (code >= 500) {
      return '桌面端服务异常：请稍后重试，或重启桌面端再试。';
    }
    return msg || ('配对失败（HTTP ' + code + '）：请重试。');
  }

  function normalizePairCode(raw) {
    return String(raw || '').toUpperCase().replace(/[^A-Z0-9]/g, '');
  }

  function isValidPairCode(raw) {
    return normalizePairCode(raw).length === 8;
  }

  function formatPairCode(raw) {
    var clean = normalizePairCode(raw);
    if (clean.length <= 4) return clean;
    return clean.slice(0, 4) + '-' + clean.slice(4, 8);
  }

  /// 宽屏布局判定（与 CSS 媒体查询保持一致）：
  /// `min-width: 900px`，或横屏且 `min-width: 768px`。手机竖屏保持单栏。
  function isWideLayout(width, height) {
    var w = Number(width) || 0;
    var h = Number(height) || 0;
    return w >= 900 || (w >= 768 && w > h);
  }

  /// 设备页「API 状态」文案：绿色时也带上 detail（最近事件），unknown/null 返回空串。
  function providerStatusText(status) {
    if (!status || !status.indicator) return '';
    var indicator = String(status.indicator).toLowerCase();
    if (indicator === 'unknown') return '';
    var text = (status.provider || 'API') + ' · ' + (status.label || '');
    if (status.detail) text += '（' + status.detail + '）';
    return text;
  }

  return {
    RECONNECT_STEPS: RECONNECT_STEPS,
    HEARTBEAT_INTERVAL_MS: HEARTBEAT_INTERVAL_MS,
    MAX_MISSED_PONGS: MAX_MISSED_PONGS,
    SILENT_RETRIES: SILENT_RETRIES,
    nextReconnectDelay: nextReconnectDelay,
    shouldDeclareDisconnected: shouldDeclareDisconnected,
    shouldShowDisconnectBanner: shouldShowDisconnectBanner,
    pairFailureMessage: pairFailureMessage,
    normalizePairCode: normalizePairCode,
    isValidPairCode: isValidPairCode,
    formatPairCode: formatPairCode,
    providerStatusText: providerStatusText,
    isWideLayout: isWideLayout
  };
});
