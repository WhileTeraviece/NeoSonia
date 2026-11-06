/* NeoSonia 远程 SPA（v0.5）
 * 零依赖、离线可用、不引用任何外部资源。
 * 事件协议：WebSocket 推送 {"type": "...", "data": ...}。
 *
 * v0.5 重点（设备配对体验 + 连接稳定性）：
 *  - 扫码 /pair#<code> 自动填码 + 自动配对，失败分类文案，手动输入兜底；
 *  - 已有 token 先验证再直接进 app（不停在配对页）；401 自动回配对页并提示；
 *  - WS 25s 心跳、连续 3 次无响应才判断开；断线先静默重试 2 次，再显示横幅；
 *    退避 1s→2s→5s→10s→15s，重连后恢复会话/未读/审查队列；
 *  - 设备页显示连接状态（在线/重连中/离线）与「断开本设备」。 */
(function () {
  'use strict';

  var TOKEN_KEY = 'neosonia.token';
  var DEVICE_KEY = 'neosonia.device';
  var MAX_RENDERED = 200;   // 首屏最多渲染多少条消息
  var PAGE_SIZE = 100;      // 「加载更早」一页多少条
  var STREAM_FRAME_MS = 33; // 流式渲染合批上限（约 30fps）
  var REVIEW_POLL_MS = 20000;
  var DEFAULT_DEVICE_NAME = '我的设备';

  var RISK_LABELS = {
    low: '低风险', medium: '中风险', high: '高风险', critical: '严重', '': '待审查'
  };

  // 纯逻辑（心跳退避/配对文案）来自 /spa-core.js，可被 Node 单测。
  var core = window.NeoSoniaCore;
  if (!core) {
    // spa-core.js 加载失败时的最小兜底，保证页面仍可手动配对。
    core = {
      RECONNECT_STEPS: [1000, 2000, 5000, 10000, 15000],
      HEARTBEAT_INTERVAL_MS: 25000,
      MAX_MISSED_PONGS: 3,
      SILENT_RETRIES: 2,
      nextReconnectDelay: function (n) { return [1000, 2000, 5000, 10000, 15000][Math.min(Math.max(1, n) - 1, 4)]; },
      shouldDeclareDisconnected: function (n) { return n >= 3; },
      shouldShowDisconnectBanner: function (n) { return n > 2; },
      pairFailureMessage: function (s, e) { return e || ('配对失败（HTTP ' + (s || '?') + '）'); },
      normalizePairCode: function (v) { return String(v || '').toUpperCase().replace(/[^A-Z0-9]/g, ''); },
      isValidPairCode: function (v) { return String(v || '').replace(/[^A-Za-z0-9]/g, '').length === 8; },
      formatPairCode: function (v) { var c = String(v || '').toUpperCase().replace(/[^A-Z0-9]/g, ''); return c.length > 4 ? c.slice(0, 4) + '-' + c.slice(4, 8) : c; }
    };
  }

  // Markdown/表格/公式渲染核心（纯逻辑，见 /md-core.js；加载失败时退化为纯文本）。
  var md = window.NeoSoniaMd;
  if (!md) {
    md = {
      renderMarkdown: function (s) { return '<p>' + esc(String(s || '')).replace(/\n/g, '<br>') + '</p>'; },
      renderMath: function () { return ''; },
      splitAttachments: function (t) { return { text: String(t || '').trim(), attachments: [] }; }
    };
  }

  // Performance API 计时（做 native vs remote 性能对比用）。
  var perf = {
    boot: performance.now(),
    firstPaint: 0,
    appVisible: 0,
    sessionsLoaded: 0,
    sessionsFetchMs: 0,
    wsConnected: 0,
    firstMessageRender: 0
  };

  var state = {
    token: '',
    device: null,
    server: null,
    mode: '',
    modeLabel: '',
    providerStatus: null,
    statusTimer: 0,
    statusRetryTimer: 0,
    statusRetries: 0,
    ws: null,
    connState: 'offline', // offline / connecting / online / reconnecting
    wsAttempts: 0,
    missedPongs: 0,
    hbTimer: 0,
    reconnectTimer: 0,
    wsConnections: 0,
    lastHeartbeatMs: 0,
    heartbeatTimer: 0,
    view: 'list',
    sessions: [],
    sessionsLoading: false,
    sendBusy: false,
    current: null,
    currentTitle: '',
    msgData: [],
    msgHasEarlier: false,
    msgLoadingEarlier: false,
    turn: null,
    pendingUserText: null,
    review: { items: [], expanded: false },
    shotCache: {},
    uploads: [],
    unread: {},
    refreshTimer: 0,
    reviewPollTimer: 0,
    searchTimer: 0,
    stickBottom: true,
    scrollQueued: false,
    autoPairStarted: false,
    autoOpenedOnce: false
  };

  var $ = function (id) { return document.getElementById(id); };
  var messagesEl = null;

  /* ================= 基础工具 ================= */

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function show(node, yes) {
    if (!node) return;
    node.classList.toggle('hidden', !yes);
  }

  function esc(s) {
    return String(s === undefined || s === null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  var toastTimer = 0;
  function toast(msg) {
    var node = $('toast');
    if (!node) return;
    node.textContent = msg;
    show(node, true);
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { show(node, false); }, 2800);
  }

  function pad2(n) { return n < 10 ? '0' + n : '' + n; }

  function fmtTime(ms) {
    if (!ms) return '';
    var d = new Date(ms);
    var diff = Date.now() - ms;
    if (diff >= 0 && diff < 60000) return '刚刚';
    if (diff >= 0 && diff < 3600000) return Math.floor(diff / 60000) + ' 分钟前';
    var today = new Date();
    var sameDay = today.getFullYear() === d.getFullYear() && today.getMonth() === d.getMonth() && today.getDate() === d.getDate();
    if (sameDay) return '今天 ' + pad2(d.getHours()) + ':' + pad2(d.getMinutes());
    return d.getFullYear() + '-' + pad2(d.getMonth() + 1) + '-' + pad2(d.getDate()) + ' ' + pad2(d.getHours()) + ':' + pad2(d.getMinutes());
  }

  function fmtRelative(ms) {
    if (!ms) return '未知';
    var diff = Math.max(0, Date.now() - ms);
    if (diff < 60000) return '刚刚';
    if (diff < 3600000) return Math.floor(diff / 60000) + ' 分钟前';
    if (diff < 86400000) return Math.floor(diff / 3600000) + ' 小时前';
    return Math.floor(diff / 86400000) + ' 天前';
  }

  function truncate(s, n) {
    s = String(s === undefined || s === null ? '' : s);
    return s.length > n ? s.slice(0, n) + '…' : s;
  }

  function enc(s) { return encodeURIComponent(String(s)); }

  function perfText(ms) { return ms > 0 ? Math.round(ms) + ' ms' : '—'; }

  function renderPerf() {
    var pairs = [
      ['perf-first-paint', perf.firstPaint],
      ['perf-app', perf.appVisible],
      ['perf-sessions', perf.sessionsLoaded],
      ['perf-fetch', perf.sessionsFetchMs],
      ['perf-ws', perf.wsConnected],
      ['perf-render', perf.firstMessageRender]
    ];
    pairs.forEach(function (p) {
      var node = $(p[0]);
      if (node) node.textContent = perfText(p[1]);
    });
    window.__neosoniaPerf = {
      t_first_paint: perf.firstPaint,
      t_app_ready: perf.appVisible,
      t_sessions_loaded: perf.sessionsLoaded,
      t_sessions_fetch_ms: perf.sessionsFetchMs,
      t_ws_connected: perf.wsConnected,
      t_first_message_render: perf.firstMessageRender
    };
  }

  function recordFirstPaint() {
    try {
      var paints = performance.getEntriesByType('paint') || [];
      paints.forEach(function (p) {
        if (!perf.firstPaint && p.name === 'first-paint') perf.firstPaint = p.startTime;
      });
    } catch (e) {}
    if (!perf.firstPaint) {
      requestAnimationFrame(function (ts) {
        if (!perf.firstPaint) perf.firstPaint = ts;
        renderPerf();
      });
    } else {
      renderPerf();
    }
  }

  /* ================= 本地存储 ================= */

  function loadToken() { try { return localStorage.getItem(TOKEN_KEY) || ''; } catch (e) { return ''; } }
  function saveToken(t) { try { localStorage.setItem(TOKEN_KEY, t); } catch (e) {} }
  function loadDevice() {
    try {
      var raw = localStorage.getItem(DEVICE_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch (e) { return null; }
  }
  function saveDevice(d) { try { localStorage.setItem(DEVICE_KEY, JSON.stringify(d || {})); } catch (e) {} }
  function clearAuth() {
    state.token = '';
    state.device = null;
    try { localStorage.removeItem(TOKEN_KEY); localStorage.removeItem(DEVICE_KEY); } catch (e) {}
  }

  // 仅测试用：sessionStorage 里放 neosonia.debug.offline=1 可模拟 WS 连不上，
  // 用于验证"断线重连"横幅（正常用户不会触发）。
  function debugOfflineRequested() {
    try { return sessionStorage.getItem('neosonia.debug.offline') === '1'; } catch (e) { return false; }
  }

  /* ================= HTTP ================= */

  function api(path, options) {
    options = options || {};
    var headers = {};
    if (state.token) headers['Authorization'] = 'Bearer ' + state.token;
    var body = options.body;
    if (options.json !== undefined) {
      headers['Content-Type'] = 'application/json';
      body = JSON.stringify(options.json);
    }
    return fetch(path, {
      method: options.method || 'GET',
      headers: headers,
      body: body,
      cache: 'no-store'
    }).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
        if (!res.ok) {
          var err = new Error((data && data.error) || ('请求失败（HTTP ' + res.status + '）'));
          err.status = res.status;
          err.body = data;
          if (res.status === 401 && !options.skipAuthRedirect) handleUnauthorized();
          throw err;
        }
        return data;
      });
    }).catch(function (err) {
      if (err && typeof err.status === 'number') throw err;
      var net = new Error('网络连接失败');
      net.status = 0;
      net.network = true;
      throw net;
    });
  }

  function handleUnauthorized() {
    if (state.view === 'pair') return;
    clearAuth();
    stopHeartbeat();
    disconnectWs();
    showPairView('设备授权已失效，请在桌面端查看新配对码。');
  }

  /* ================= 视图切换 / 错误态 ================= */

  /// 宽屏布局（桌面浏览器 / 平板横屏）：与 CSS 媒体查询保持一致。
  function isWideLayout() {
    return core.isWideLayout(window.innerWidth, window.innerHeight);
  }

  function setView(name) {
    var wide = isWideLayout();
    // 宽屏：左栏（会话列表）常驻，右栏在 chat/device 之间切换。
    // 'list' 在宽屏下表示"右栏回到对话"（没有当前会话就是空态），左栏不受影响。
    if (wide && name === 'list') name = 'chat';
    state.view = name;
    show($('list-view'), wide || name === 'list');
    show($('chat-view'), name === 'chat');
    show($('device-view'), name === 'device');
    // 宽屏返回按钮只控制右栏：设备页 → 对话；聊天页不需要返回（左栏常驻）。
    show($('back-btn'), wide ? name === 'device' : name !== 'list');
    if (name === 'list') $('top-title').textContent = '对话';
    if (name === 'chat') {
      $('top-title').textContent = state.currentTitle || '对话';
    }
    if (name === 'device') { $('top-title').textContent = '设备'; renderPerf(); renderDevConn(); renderHeartbeat(); }
  }

  function renderError(host, message, retry) {
    if (!host) return;
    host.textContent = '';
    host.appendChild(el('div', 'error-text', message || '请求失败，请重试。'));
    if (retry) {
      var btn = el('button', 'btn btn-ghost', '重试');
      btn.type = 'button';
      btn.addEventListener('click', function () { show(host, false); retry(); });
      host.appendChild(btn);
    }
    show(host, true);
  }

  function showPairView(note) {
    show($('boot'), false);
    show($('app-view'), false);
    show($('pair-view'), true);
    var noteEl = $('pair-note');
    if (note) { noteEl.textContent = note; show(noteEl, true); } else { show(noteEl, false); }
    var nameInput = $('pair-name');
    if (!nameInput.value) nameInput.value = DEFAULT_DEVICE_NAME;
    setTimeout(function () { try { nameInput.focus(); } catch (e) {} }, 60);
  }

  function showPairError(msg) {
    var node = $('pair-error');
    node.textContent = msg || '';
    show(node, !!(msg && msg.length));
  }

  /* ================= 配对（自动 + 手动） ================= */

  function postPair(payload, onOk) {
    fetch('/api/pair', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(payload)
    }).then(function (res) {
      return res.text().then(function (text) {
        var data = null;
        try { data = text ? JSON.parse(text) : null; } catch (e) { data = null; }
        if (!res.ok) {
          var err = new Error((data && data.error) || ('配对失败（HTTP ' + res.status + '）'));
          err.status = res.status;
          err.body = data;
          throw err;
        }
        return data;
      });
    }).then(onOk).catch(function (err) {
      pairFailed(err);
    });
  }

  function pairFailed(err) {
    var status = err && typeof err.status === 'number' ? err.status : 0;
    var serverMsg = (err && err.body && err.body.error) ? err.body.error : '';
    var msg = core.pairFailureMessage(status, serverMsg);
    show($('pair-auto'), false);
    var btn = $('pair-btn');
    btn.disabled = false;
    btn.textContent = '重新配对';
    showPairError(msg);
    try { $('pair-code').focus(); } catch (e) {}
  }

  function onPaired(data) {
    if (!data || !data.token) {
      pairFailed(new Error('配对响应缺少 token。'));
      return;
    }
    state.token = data.token;
    state.device = data.device || null;
    saveToken(data.token);
    saveDevice(state.device);
    try { history.replaceState(null, '', '/'); } catch (e) { location.hash = ''; }
    show($('pair-auto'), false);
    showPairError('');
    var btn = $('pair-btn');
    btn.disabled = false;
    btn.textContent = '开始配对';
    if (!perf.appVisible) perf.appVisible = performance.now();
    enterApp();
  }

  function autoPair(code) {
    if (state.autoPairStarted) return;
    if (!core.isValidPairCode(code)) {
      showPairView('配对码不完整：请重新扫码，或在下面手动输入桌面端显示的 8 位配对码。');
      return;
    }
    state.autoPairStarted = true;
    show($('pair-auto'), true);
    var btn = $('pair-btn');
    btn.disabled = true;
    btn.textContent = '正在自动配对…';
    postPair({ code: core.normalizePairCode(code), name: DEFAULT_DEVICE_NAME }, onPaired);
  }

  function manualPair() {
    var raw = $('pair-code').value;
    if (!core.isValidPairCode(raw)) {
      showPairError('请填写完整的 8 位配对码（可含连字符）。');
      return;
    }
    var name = $('pair-name').value.trim() || DEFAULT_DEVICE_NAME;
    state.autoPairStarted = true;
    showPairError('');
    show($('pair-auto'), false);
    var btn = $('pair-btn');
    btn.disabled = true;
    btn.textContent = '正在配对…';
    postPair({ code: core.normalizePairCode(raw), name: name }, onPaired);
  }

  function unpair() {
    if (!window.confirm('断开本设备？之后需要重新输入配对码才能连接。')) return;
    clearAuth();
    stopHeartbeat();
    disconnectWs();
    state.sessions = [];
    state.msgData = [];
    state.unread = {};
    state.autoPairStarted = false;
    updateTitleUnread();
    showPairView('已断开本设备，请重新输入配对码。');
  }

  /* ================= 连接状态 UI ================= */

  function connLabel() {
    if (state.connState === 'online') return '在线';
    if (state.connState === 'reconnecting' || state.connState === 'connecting') return '重连中';
    return '离线';
  }

  function renderDevConn() {
    var node = $('dev-conn');
    if (!node) return;
    node.textContent = connLabel();
  }

  /// 最后心跳：最近一次收到任何 WS 帧的时间（相对当前时刻）。
  function renderHeartbeat() {
    var node = $('dev-heartbeat');
    if (!node) return;
    node.textContent = state.lastHeartbeatMs ? fmtRelative(state.lastHeartbeatMs) : '—';
  }

  function setConnState(next) {
    state.connState = next;
    var kind = next === 'online' ? 'on' : (next === 'offline' ? 'off' : 'wait');
    connDot(kind);
    if (next === 'online') hideConnBanner();
    renderDevConn();
  }

  function showConnBanner(text, retryable) {
    $('conn-banner-text').textContent = text;
    show($('conn-retry'), retryable !== false);
    show($('conn-banner'), true);
  }

  function hideConnBanner() { show($('conn-banner'), false); }

  function connDot(kind) {
    var dot = $('conn-dot');
    if (!dot) return;
    dot.className = 'dot ' + (kind === 'on' ? 'dot-on' : kind === 'wait' ? 'dot-wait' : 'dot-off');
    dot.title = kind === 'on' ? '已连接' : kind === 'wait' ? '连接中' : '未连接';
  }

  /* ================= 会话列表 / 未读 ================= */

  function clearUnread(convId) {
    if (!convId || !state.unread[convId]) return;
    delete state.unread[convId];
    renderSessions();
    updateTitleUnread();
  }

  function markUnread(convId) {
    if (!convId) return;
    if (state.view === 'chat' && state.current === convId && !document.hidden) return;
    state.unread[convId] = (state.unread[convId] || 0) + 1;
    renderSessions();
    updateTitleUnread();
  }

  function totalUnread() {
    var n = 0;
    Object.keys(state.unread).forEach(function (k) { n += state.unread[k] || 0; });
    return n;
  }

  function updateTitleUnread() {
    var n = totalUnread();
    document.title = n > 0 ? 'NeoSonia 远程（' + n + '）' : 'NeoSonia 远程';
  }

  function loadSessions(showSkeleton) {
    if (state.sessionsLoading) return Promise.resolve();
    state.sessionsLoading = true;
    if (showSkeleton !== false) {
      show($('list-skeleton'), true);
      show($('list-error'), false);
    }
    return api('/api/sessions?limit=100').then(function (list) {
      state.sessions = Array.isArray(list) ? list : [];
      renderSessions();
      show($('list-skeleton'), false);
      show($('list-error'), false);
      show($('list-empty'), state.sessions.length === 0);
      if (!perf.sessionsLoaded) {
        perf.sessionsLoaded = performance.now();
        perf.sessionsFetchMs = Math.max(0, perf.sessionsLoaded - (perf.appVisible || perf.sessionsLoaded));
        renderPerf();
      }
    }).catch(function (err) {
      show($('list-skeleton'), false);
      renderError($('list-error'), '读取会话列表失败：' + err.message, function () { loadSessions(true); });
    }).then(function () {
      state.sessionsLoading = false;
    });
  }

  function renderSessions() {
    var host = $('session-list');
    var frag = document.createDocumentFragment();
    (state.sessions || []).forEach(function (s) {
      var item = el('div', 'session-item');
      item.setAttribute('role', 'button');
      item.tabIndex = 0;
      if (s.id === state.current) item.classList.add('active');
      var titleRow = el('div', 'session-title-row');
      titleRow.appendChild(el('span', 'session-title', s.title || '未命名对话'));
      if (state.unread[s.id]) titleRow.appendChild(el('span', 'unread-dot'));
      item.appendChild(titleRow);
      if (s.preview) item.appendChild(el('span', 'session-preview', s.preview));
      var meta = el('span', 'session-meta');
      meta.appendChild(el('span', '', (s.message_count || 0) + ' 条消息'));
      meta.appendChild(el('span', '', fmtTime(s.updated_at_ms)));
      item.appendChild(meta);
      var del = el('button', 'icon-btn session-del', '删除');
      del.type = 'button';
      del.addEventListener('click', function (ev) { ev.stopPropagation(); deleteSession(s); });
      item.appendChild(del);
      function open() { openSession(s.id, s.title || ''); }
      item.addEventListener('click', open);
      item.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); open(); }
      });
      frag.appendChild(item);
    });
    host.textContent = '';
    host.appendChild(frag);
    show($('list-empty'), (state.sessions || []).length === 0);
    updateTitleUnread();
  }

  function deleteSession(s) {
    if (!window.confirm('删除对话「' + (s.title || '未命名对话') + '」？删除后无法恢复。')) return;
    api('/api/sessions/' + enc(s.id) + '/delete', { method: 'POST' }).then(function () {
      if (state.current === s.id) {
        state.current = null;
        state.currentTitle = '';
        state.msgData = [];
        messagesEl.textContent = '';
        setView('list');
      }
      toast('已删除');
      loadSessions(false);
    }).catch(function (err) { toast('删除失败：' + err.message); });
  }

  /* ================= 消息：分页/渲染 ================= */

  function currentLastSeq() {
    if (!state.msgData.length) return 0;
    return Number(state.msgData[state.msgData.length - 1].seq) || 0;
  }

  function openSession(id, title) {
    state.current = id;
    state.currentTitle = title || '';
    clearUnread(id);
    $('top-title').textContent = title || '对话';
    setView('chat');
    clearTurn();
    state.msgData = [];
    state.msgHasEarlier = false;
    state.stickBottom = true;
    messagesEl.textContent = '';
    show($('chat-skeleton'), true);
    show($('chat-error'), false);
    show($('chat-empty'), false);
    show($('jump-btn'), false);
    api('/api/sessions/' + enc(id) + '?limit=' + MAX_RENDERED).then(function (list) {
      list = Array.isArray(list) ? list : [];
      state.msgData = list;
      state.msgHasEarlier = list.length > 0 && Number(list[0].seq) > 1;
      renderMessageWindow();
      show($('chat-skeleton'), false);
      show($('chat-empty'), list.length === 0);
      if (!perf.firstMessageRender && list.length) { perf.firstMessageRender = performance.now(); renderPerf(); }
      scrollBottom(true);
    }).catch(function (err) {
      show($('chat-skeleton'), false);
      renderError($('chat-error'), '读取会话失败：' + err.message, function () { openSession(id, title); });
    });
  }

  function refreshCurrentConversation() {
    var id = state.current;
    if (!id) return Promise.resolve();
    return api('/api/sessions/' + enc(id) + '?limit=' + MAX_RENDERED).then(function (list) {
      list = Array.isArray(list) ? list : [];
      state.msgData = list;
      state.msgHasEarlier = list.length > 0 && Number(list[0].seq) > 1;
      if (state.view === 'chat') {
        renderMessageWindow();
        scrollBottom(state.stickBottom);
      }
    }).catch(function () {});
  }

  function renderMessageWindow() {
    var frag = document.createDocumentFragment();
    if (state.msgHasEarlier) {
      var wrap = el('div', 'earlier-wrap');
      var btn = el('button', 'btn btn-ghost', state.msgLoadingEarlier ? '正在加载…' : '加载更早的消息');
      btn.type = 'button';
      btn.disabled = state.msgLoadingEarlier;
      btn.addEventListener('click', loadEarlier);
      wrap.appendChild(btn);
      frag.appendChild(wrap);
    }
    for (var i = 0; i < state.msgData.length; i++) {
      frag.appendChild(historyMessage(state.msgData[i]));
    }
    messagesEl.textContent = '';
    messagesEl.appendChild(frag);
    if (state.stickBottom) messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  function loadEarlier() {
    if (state.msgLoadingEarlier || !state.msgData.length || !state.current) return;
    state.msgLoadingEarlier = true;
    renderMessageWindow();
    var firstSeq = Number(state.msgData[0].seq);
    var scrollBefore = messagesEl.scrollHeight;
    var topBefore = messagesEl.scrollTop;
    api('/api/sessions/' + enc(state.current) + '?before=' + firstSeq + '&limit=' + PAGE_SIZE).then(function (older) {
      older = Array.isArray(older) ? older : [];
      state.msgLoadingEarlier = false;
      if (!older.length) {
        state.msgHasEarlier = false;
        renderMessageWindow();
        return;
      }
      state.msgData = older.concat(state.msgData);
      state.msgHasEarlier = Number(older[0].seq) > 1;
      renderMessageWindow();
      messagesEl.scrollTop = messagesEl.scrollHeight - scrollBefore + topBefore;
      state.stickBottom = false;
    }).catch(function (err) {
      state.msgLoadingEarlier = false;
      renderMessageWindow();
      toast('加载更早消息失败：' + err.message + '（点顶部按钮重试）');
    });
  }

  function historyMessage(m) {
    if (!m) return el('div', 'msg-notice', '');
    if (m.role === 'user') return renderUserMessage(m);

    if (m.role === 'assistant') {
      var node = el('div', 'msg msg-assistant');
      var container = el('div', 'msg-body');
      if (m.reasoning_content) container.appendChild(reasoningBlock(m.reasoning_content));
      if (m.tool_calls && m.tool_calls.length) {
        m.tool_calls.forEach(function (c) {
          var name = (c.function && c.function.name) || 'tool';
          container.appendChild(el('div', 'session-meta mono', '调用工具：' + name + '（' + truncate(c.function && c.function.arguments, 120) + '）'));
        });
      }
      var md = el('div', 'md');
      md.innerHTML = renderMarkdown(m.content || '');
      mountScreenshots(md, m.content || '');
      container.appendChild(md);
      node.appendChild(container);
      // v0.7.4：助手消息底部右侧「复制完整回复」（纯文本，含代码/公式源文本）。
      node.appendChild(replyActions(function () { return m.content || ''; }));
      return node;
    }
    if (m.role === 'tool') {
      var card = el('div', 'tool-card');
      var head = el('div', 'tool-head');
      head.appendChild(el('span', 'tool-name', m.name || 'tool'));
      head.appendChild(el('span', 'tool-status ' + (m.tool_ok === false ? 'fail' : 'ok'), m.tool_ok === false ? '失败' : '完成'));
      card.appendChild(head);
      if (m.content) {
        card.appendChild(el('div', 'tool-output plain', truncate(m.content, 6000)));
        mountScreenshots(card, m.content);
      }
      return card;
    }
    return el('div', 'msg-notice', (m.content || '').slice(0, 400));
  }

  function reasoningBlock(text) {
    var details = document.createElement('details');
    details.className = 'reasoning';
    var summary = document.createElement('summary');
    summary.textContent = '思考过程';
    details.appendChild(summary);
    details.appendChild(document.createTextNode(text));
    details.open = false;
    return details;
  }

  /* ================= 滚动：rAF 节流 + 上滚暂停贴底 ================= */

  function onMessagesScroll() {
    if (!messagesEl) return;
    var dist = messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight;
    state.stickBottom = dist < 120;
    show($('jump-btn'), !state.stickBottom && messagesEl.scrollHeight > messagesEl.clientHeight && state.view === 'chat');
  }

  function scrollBottom(force) {
    if (!messagesEl) return;
    if (!force && !state.stickBottom) {
      show($('jump-btn'), messagesEl.scrollHeight > messagesEl.clientHeight);
      return;
    }
    if (state.scrollQueued) return;
    state.scrollQueued = true;
    requestAnimationFrame(function () {
      state.scrollQueued = false;
      if (!state.stickBottom && !force) return;
      messagesEl.scrollTop = messagesEl.scrollHeight;
      show($('jump-btn'), false);
    });
  }

  function jumpToBottom() {
    state.stickBottom = true;
    scrollBottom(true);
  }

  /* ================= Markdown 渲染（基础子集 + 公式） ================= */

  function legacyRenderMarkdown(src) {
    var text = String(src === undefined || src === null ? '' : src);
    var tokens = [];

    function stash(kind, body) {
      tokens.push({ kind: kind, body: body });
      return '\u0001' + (tokens.length - 1) + '\u0001';
    }

    text = text.replace(/```([A-Za-z0-9_+\-]*)\r?\n?([\s\S]*?)```/g, function (m, lang, body) {
      return stash('code', { lang: lang, body: body.replace(/\r?\n$/, '') });
    });
    text = text.replace(/`([^`\n]+)`/g, function (m, body) { return stash('icode', body); });
    text = text.replace(/\$\$([\s\S]+?)\$\$/g, function (m, body) { return stash('mathblock', body); });
    text = text.replace(/\$([^\s$][^$\n]*?[^\s$])\$/g, function (m, body) { return stash('math', body); });

    text = esc(text);
    var lines = text.split('\n');
    var out = [];
    var para = [];
    var listTag = null;

    function flushPara() {
      if (!para.length) return;
      out.push('<p>' + inlineMarkup(para.join('\n').replace(/\n/g, '<br>')) + '</p>');
      para = [];
    }
    function closeList() {
      if (!listTag) return;
      out.push('</' + listTag + '>');
      listTag = null;
    }

    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      var m;
      if (/^\s*$/.test(line)) { flushPara(); closeList(); continue; }
      if ((m = line.match(/^(#{1,4})\s+(.*)$/))) {
        flushPara(); closeList();
        var level = m[1].length;
        out.push('<h' + level + '>' + inlineMarkup(m[2]) + '</h' + level + '>');
        continue;
      }
      if (/^\s*(?:---+|\*\*\*+|___+)\s*$/.test(line)) { flushPara(); closeList(); out.push('<hr class="md-hr">'); continue; }
      if ((m = line.match(/^&gt;\s?(.*)$/))) { flushPara(); closeList(); out.push('<blockquote>' + inlineMarkup(m[1]) + '</blockquote>'); continue; }
      if ((m = line.match(/^\s*[-*+]\s+(.*)$/))) {
        flushPara();
        if (listTag !== 'ul') { closeList(); out.push('<ul>'); listTag = 'ul'; }
        out.push('<li>' + inlineMarkup(m[1]) + '</li>');
        continue;
      }
      if ((m = line.match(/^\s*\d+[.)]\s+(.*)$/))) {
        flushPara();
        if (listTag !== 'ol') { closeList(); out.push('<ol>'); listTag = 'ol'; }
        out.push('<li>' + inlineMarkup(m[1]) + '</li>');
        continue;
      }
      closeList();
      para.push(line);
    }
    flushPara();
    closeList();

    return out.join('\n').replace(/\u0001(\d+)\u0001/g, function (all, idx) {
      var t = tokens[parseInt(idx, 10)];
      if (!t) return '';
      if (t.kind === 'code') {
        var lang = t.body.lang ? ' class="lang-' + esc(t.body.lang) + '"' : '';
        return '<pre><code' + lang + '>' + esc(t.body.body) + '</code></pre>';
      }
      if (t.kind === 'icode') return '<code class="inline">' + esc(t.body) + '</code>';
      if (t.kind === 'math') return '<span class="math">' + esc(t.body) + '</span>';
      return '<div class="math-block">' + esc(t.body) + '</div>';
    });
  }

  function inlineMarkup(s) {
    s = s.replace(/\*\*([^*\n]+?)\*\*/g, '<strong>$1</strong>');
    s = s.replace(/(^|[^*\w])\*([^*\n]+?)\*(?!\*)/g, '$1<em>$2</em>');
    s = s.replace(/\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)/g, function (all, label, url) {
      return '<a href="' + url + '" target="_blank" rel="noopener noreferrer">' + label + '</a>';
    });
    return s;
  }

  /* ================= 聊天：流式渲染 ================= */

  function isChatVisible() { return state.view === 'chat'; }

  function appendUserBubble(text) {
    var node = el('div', 'msg msg-user');
    var body = el('div', 'msg-body');
    body.textContent = text;
    node.appendChild(body);
    messagesEl.appendChild(node);
    show($('chat-empty'), false);
    scrollBottom(true);
  }

  function addNotice(kind, text) {
    if (!isChatVisible() && kind !== 'error') { toast(text); return; }
    var cls = 'msg-notice';
    if (kind === 'error') cls += ' msg-error';
    else if (kind === 'warn') cls += ' msg-warn';
    messagesEl.appendChild(el('div', cls, text));
    scrollBottom();
  }

  function setStatus(text) {
    var node = $('status-line');
    if (!text) { show(node, false); return; }
    node.textContent = text;
    show(node, true);
  }

  function setBusyUi(busy) {
    show($('stop-btn'), busy);
    if (busy) $('send-btn').disabled = true;
    else updateSendEnabled();
    if (!busy) show($('status-line'), false);
  }

  function createAssistantCard() {
    var node = el('div', 'msg msg-assistant');
    var body = el('div', 'msg-body');
    var md = el('div', 'md');
    body.appendChild(md);
    node.appendChild(body);
    messagesEl.appendChild(node);
    state.turn = {
      node: node, body: body, md: md,
      text: '', reasoning: '',
      reasoningBox: null, reasoningText: null,
      tools: {}, paintQueued: false, lastPaint: 0
    };
  }

  function ensureReasoningBox() {
    if (!state.turn || state.turn.reasoningBox) return;
    var box = document.createElement('details');
    box.className = 'reasoning';
    var summary = document.createElement('summary');
    summary.textContent = '思考过程';
    box.appendChild(summary);
    var txt = el('div', 'reasoning-text');
    box.appendChild(txt);
    state.turn.body.insertBefore(box, state.turn.md);
    state.turn.reasoningBox = box;
    state.turn.reasoningText = txt;
  }

  function scheduleTurnPaint() {
    if (!state.turn || state.turn.paintQueued) return;
    state.turn.paintQueued = true;
    requestAnimationFrame(function () {
      if (!state.turn) return;
      state.turn.paintQueued = false;
      var now = performance.now();
      if (now - state.turn.lastPaint < STREAM_FRAME_MS) { scheduleTurnPaint(); return; }
      state.turn.lastPaint = now;
      state.turn.md.innerHTML = renderMarkdown(state.turn.text);
      mountScreenshots(state.turn.md, state.turn.text);
      if (state.turn.reasoningText) state.turn.reasoningText.textContent = state.turn.reasoning;
      scrollBottom();
    });
  }

  function appendText(chunk) {
    if (!state.turn) createAssistantCard();
    state.turn.text += chunk;
    scheduleTurnPaint();
  }

  function appendReasoning(chunk) {
    if (!state.turn) createAssistantCard();
    state.turn.reasoning += chunk;
    ensureReasoningBox();
    scheduleTurnPaint();
  }

  function paintTurnNow() {
    if (!state.turn) return;
    state.turn.lastPaint = performance.now();
    state.turn.md.innerHTML = renderMarkdown(state.turn.text);
    mountScreenshots(state.turn.md, state.turn.text);
    if (state.turn.reasoningText) state.turn.reasoningText.textContent = state.turn.reasoning;
    scrollBottom();
  }

  function finalizeAssistant(text, reasoning) {
    if (!state.turn) createAssistantCard();
    if (typeof text === 'string' && text.length) state.turn.text = text;
    if (typeof reasoning === 'string' && reasoning.length) {
      state.turn.reasoning = reasoning;
      ensureReasoningBox();
    }
    paintTurnNow();
    // 流式输出中不显示复制按钮；本条完整回复落定后加一次（避免重复）。
    if (!state.turn.replyRow) {
      var snapshot = state.turn.text;
      state.turn.replyRow = replyActions(function () {
        return state.turn ? state.turn.text : snapshot;
      });
      state.turn.node.appendChild(state.turn.replyRow);
      scrollBottom();
    }
  }

  function beginTurn(userText) {
    setBusyUi(true);
    markUnread(state.current);
    if (!isChatVisible()) {
      scheduleSessionRefresh();
      toast('NeoSonia 开始执行任务');
      loadReviewPending();
      return;
    }
    if (state.pendingUserText !== null && state.pendingUserText === userText) {
      state.pendingUserText = null;
    } else {
      appendUserBubble(userText || '');
    }
    createAssistantCard();
    setStatus('正在思考…');
    scrollBottom(true);
  }

  function endTurn(cancelled) {
    setBusyUi(false);
    state.sendBusy = false;
    updateSendEnabled();
    if (cancelled) addNotice('warn', '已停止生成。');
    if (state.turn && !state.turn.text && !Object.keys(state.turn.tools).length) {
      addNotice('notice', '本轮没有产生文字回复。');
    }
    state.turn = null;
    scheduleSessionRefresh();
    loadReviewPending();
  }

  function clearTurn() {
    state.turn = null;
    setBusyUi(false);
    state.sendBusy = false;
    updateSendEnabled();
    show($('status-line'), false);
  }

  function scheduleSessionRefresh() {
    clearTimeout(state.refreshTimer);
    state.refreshTimer = setTimeout(function () { loadSessions(false); }, 700);
  }

  function addToolStarted(d) {
    if (!isChatVisible()) return;
    if (!state.turn) createAssistantCard();
    var card = el('div', 'tool-card');
    var head = el('div', 'tool-head');
    head.appendChild(el('span', 'tool-name', d.name || 'tool'));
    var status = el('span', 'tool-status', '运行中');
    head.appendChild(status);
    card.appendChild(head);
    var summary = el('div', 'tool-summary hidden');
    var output = el('div', 'tool-output hidden');
    card.appendChild(summary);
    card.appendChild(output);
    head.addEventListener('click', function () {
      if (!output.textContent) return;
      output.classList.toggle('hidden');
    });
    messagesEl.appendChild(card);
    state.turn.tools[d.call_id] = { card: card, status: status, summary: summary, output: output, outputText: '' };
    if (d.preview) { summary.textContent = d.preview; show(summary, true); }
    setStatus('正在调用工具：' + (d.name || ''));
    scrollBottom();
  }

  function appendToolOutput(d) {
    if (!state.turn) return;
    var t = state.turn.tools[d.call_id];
    if (!t) return;
    t.outputText += d.chunk || '';
    if (t.outputText.length > 40000) t.outputText = t.outputText.slice(-40000);
    t.output.textContent = t.outputText;
    show(t.output, true);
    scrollBottom();
  }

  function finishTool(d) {
    if (!state.turn) return;
    var t = state.turn.tools[d.call_id];
    if (!t) return;
    t.status.textContent = (d.ok ? '成功' : '失败') + ' · ' + fmtElapsed(d.elapsed_ms);
    t.status.className = 'tool-status ' + (d.ok ? 'ok' : 'fail');
    if (d.summary) { t.summary.textContent = d.summary; show(t.summary, true); }
    if (d.output) { t.outputText = d.output; t.output.textContent = d.output; }
    if (t.outputText) show(t.output, t.outputText.length <= 1200);
    mountScreenshots(t.card, (d.output || '') + '\n' + (d.summary || ''));
    setStatus('');
    scrollBottom();
  }

  function fmtElapsed(ms) {
    ms = Number(ms) || 0;
    if (ms < 1000) return ms + 'ms';
    if (ms < 60000) return (ms / 1000).toFixed(1) + 's';
    return Math.floor(ms / 60000) + 'm' + pad2(Math.floor((ms % 60000) / 1000)) + 's';
  }

  function sendMessage() {
    var input = $('composer-input');
    var text = input.value.trim();
    if (state.sendBusy) { toast('上一轮还在执行，先点「停止」或等它结束。'); return; }
    var done = state.uploads.filter(function (item) { return item.status === 'done'; });
    var uploading = state.uploads.some(function (item) { return item.status === 'uploading'; });
    if (!text && !done.length) {
      if (uploading) toast('附件还在上传，稍等一下');
      return;
    }
    if (!isChatVisible()) setView('chat');
    input.value = '';
    autosize(input);
    var outgoing = done.map(function (item) {
      return {
        id: item.id,
        name: item.name,
        size: humanSize(item.size),
        kind: item.kind,
        url: item.url,
        file: item.file
      };
    });
    state.pendingUserText = text || '（上传了附件）';
    appendUserMessageWithAttachments(text, outgoing);
    state.uploads = [];
    renderUploadStrip();
    state.sendBusy = true;
    updateSendEnabled();
    var payload = { text: text };
    if (state.current) payload.conv_id = state.current;
    if (outgoing.length) payload.attachments = outgoing.map(function (item) { return item.id; });
    api('/api/chat', { method: 'POST', json: payload }).then(function () {
      scheduleSessionRefresh();
    }).catch(function (err) {
      state.pendingUserText = null;
      state.sendBusy = false;
      updateSendEnabled();
      addNotice('error', '发送失败：' + err.message);
    });
  }

  function stopTurn() {
    if (state.ws && state.ws.readyState === 1) {
      state.ws.send(JSON.stringify({ type: 'cancel' }));
      setStatus('正在停止…');
    } else {
      toast('连接已断开，无法停止。');
    }
  }

  function newConversation() {
    api('/api/new', { method: 'POST' }).then(function () {
      state.current = null;
      state.currentTitle = '';
      state.msgData = [];
      state.msgHasEarlier = false;
      messagesEl.textContent = '';
      clearTurn();
      show($('chat-empty'), true);
      show($('chat-error'), false);
      $('top-title').textContent = '新对话';
      setView('chat');
      $('composer-input').focus();
      loadSessions(false);
    }).catch(function (err) { toast('新建对话失败：' + err.message); });
  }

  /* ================= Markdown 渲染入口（v0.7，委托 md-core.js） ================= */

  function renderMarkdown(src) {
    return md.renderMarkdown(src);
  }

  /* ================= 附件卡片 / 上传（v0.7） ================= */

  var MAX_UPLOADS = 8;
  var MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

  function humanSize(n) {
    n = Number(n) || 0;
    var units = ['B', 'KB', 'MB', 'GB'];
    var i = 0;
    var v = n;
    while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
    return i === 0 ? n + ' B' : v.toFixed(1) + ' ' + units[i];
  }

  function guessKind(file) {
    var mime = String((file && file.type) || '');
    if (mime.indexOf('image/') === 0) return 'image';
    if (mime.indexOf('text/') === 0) return 'text';
    return 'file';
  }

  function loadUploadBlob(url) {
    var key = 'upload:' + url;
    if (state.shotCache[key]) return Promise.resolve(state.shotCache[key]);
    return fetch(url, { headers: { Authorization: 'Bearer ' + state.token }, cache: 'no-store' })
      .then(function (res) {
        if (!res.ok) throw new Error('附件读取失败');
        return res.blob();
      })
      .then(function (blob) {
        var objectUrl = URL.createObjectURL(blob);
        state.shotCache[key] = objectUrl;
        return objectUrl;
      });
  }

  function downloadUpload(att) {
    loadUploadBlob(att.url).then(function (url) {
      var link = document.createElement('a');
      link.href = url;
      link.download = att.name || '附件';
      document.body.appendChild(link);
      link.click();
      document.body.removeChild(link);
    }).catch(function () { toast('附件下载失败'); });
  }

  function renderAttachmentCards(container, attachments) {
    (attachments || []).forEach(function (att) {
      var card = el('div', 'attach-card');
      card.setAttribute('role', 'button');
      card.tabIndex = 0;
      if (att.kind === 'image') {
        var img = el('img', 'attach-thumb');
        img.alt = att.name || '图片';
        card.appendChild(img);
        loadUploadBlob(att.url).then(function (url) { img.src = url; }).catch(function () {});
      }
      var main = el('div', 'attach-card-main');
      main.appendChild(el('div', 'attach-card-name', att.name || '附件'));
      main.appendChild(el('div', 'attach-card-meta',
        (att.kind === 'image' ? '图片' : att.kind === 'text' ? '文本' : '文件') + (att.size ? ' · ' + att.size : '')));
      card.appendChild(main);
      card.addEventListener('click', function () { downloadUpload(att); });
      card.addEventListener('keydown', function (ev) {
        if (ev.key === 'Enter' || ev.key === ' ') { ev.preventDefault(); downloadUpload(att); }
      });
      container.appendChild(card);
    });
  }

  function renderUserMessage(m) {
    var user = el('div', 'msg msg-user');
    var body = el('div', 'msg-body');
    var parsed = md.splitAttachments(m.content || '');
    body.textContent = parsed.text || (parsed.attachments.length ? '（上传了附件）' : '');
    if (m.has_image && !parsed.attachments.length) {
      body.appendChild(el('div', 'session-meta', '（含 ' + (m.image_count || 1) + ' 张图片）'));
    }
    if (parsed.attachments.length) renderAttachmentCards(body, parsed.attachments);
    user.appendChild(body);
    return user;
  }

  function appendUserMessageWithAttachments(text, attachments) {
    var node = el('div', 'msg msg-user');
    var body = el('div', 'msg-body');
    body.textContent = text || '（上传了附件）';
    attachments.forEach(function (att) {
      var card = el('div', 'attach-card');
      if (att.kind === 'image' && att.file) {
        var img = el('img', 'attach-thumb');
        img.src = URL.createObjectURL(att.file);
        card.appendChild(img);
      }
      var main = el('div', 'attach-card-main');
      main.appendChild(el('div', 'attach-card-name', att.name || '附件'));
      main.appendChild(el('div', 'attach-card-meta', (att.kind === 'image' ? '图片' : att.kind === 'text' ? '文本' : '文件') + ' · ' + att.size));
      card.appendChild(main);
      body.appendChild(card);
    });
    node.appendChild(body);
    messagesEl.appendChild(node);
    show($('chat-empty'), false);
    scrollBottom(true);
  }

  function renderUploadStrip() {
    var strip = $('attach-strip');
    var host = $('attach-items');
    if (!strip || !host) return;
    if (!state.uploads.length) {
      show(strip, false);
      host.textContent = '';
      show($('attach-progress'), false);
      updateSendEnabled();
      return;
    }
    show(strip, true);
    host.textContent = '';
    var frag = document.createDocumentFragment();
    state.uploads.forEach(function (item, index) {
      var chip = el('div', 'attach-chip' + (item.status === 'uploading' ? ' uploading' : '') + (item.status === 'error' ? ' error' : ''));
      if (item.file && item.kind === 'image') {
        var img = el('img', 'attach-thumb');
        img.src = URL.createObjectURL(item.file);
        chip.appendChild(img);
      }
      var main = el('div', 'attach-card-main');
      main.appendChild(el('div', 'attach-name', item.name));
      main.appendChild(el('div', 'attach-meta',
        item.status === 'uploading' ? ('上传中 ' + item.progress + '%')
          : item.status === 'error' ? (item.error || '上传失败')
            : humanSize(item.size)));
      chip.appendChild(main);
      if (item.status === 'error') {
        var retry = el('button', 'attach-retry', '重试');
        retry.type = 'button';
        retry.addEventListener('click', function () { retryUpload(index); });
        chip.appendChild(retry);
      } else if (item.status === 'uploading') {
        var bar = el('div', 'attach-progress');
        bar.style.width = item.progress + '%';
        chip.appendChild(bar);
      }
      var x = el('button', 'attach-x', '×');
      x.type = 'button';
      x.setAttribute('aria-label', '移除附件');
      x.addEventListener('click', function () { removeUpload(index); });
      chip.appendChild(x);
      frag.appendChild(chip);
    });
    host.appendChild(frag);
    updateSendEnabled();
  }

  function doUpload(item) {
    item.status = 'uploading';
    item.progress = 0;
    item.error = '';
    renderUploadStrip();
    var xhr = new XMLHttpRequest();
    xhr.open('POST', '/api/upload?name=' + enc(item.name), true);
    xhr.setRequestHeader('Authorization', 'Bearer ' + state.token);
    xhr.setRequestHeader('Content-Type', item.mime || 'application/octet-stream');
    xhr.upload.onprogress = function (ev) {
      if (ev.lengthComputable) {
        item.progress = Math.round(ev.loaded / ev.total * 100);
        renderUploadStrip();
      }
    };
    xhr.onload = function () {
      var data = null;
      try { data = JSON.parse(xhr.responseText || 'null'); } catch (e) {}
      if (xhr.status >= 200 && xhr.status < 300 && data && data.id) {
        item.id = data.id;
        item.url = data.url;
        item.status = 'done';
        item.progress = 100;
      } else {
        item.status = 'error';
        item.error = (data && data.error) || ('上传失败（HTTP ' + xhr.status + '）');
        if (xhr.status === 401) handleUnauthorized();
      }
      renderUploadStrip();
    };
    xhr.onerror = function () {
      item.status = 'error';
      item.error = '网络错误，点重试';
      renderUploadStrip();
    };
    xhr.send(item.file);
  }

  function retryUpload(index) {
    var item = state.uploads[index];
    if (item) doUpload(item);
  }

  function removeUpload(index) {
    state.uploads.splice(index, 1);
    renderUploadStrip();
  }

  function uploadFiles(fileList) {
    var files = Array.prototype.slice.call(fileList || []);
    if (!files.length) return;
    if (state.uploads.length >= MAX_UPLOADS) { toast('最多 ' + MAX_UPLOADS + ' 个附件'); return; }
    files = files.slice(0, MAX_UPLOADS - state.uploads.length);
    files.forEach(function (file) {
      if (file.size > MAX_UPLOAD_BYTES) { toast(file.name + ' 超过 25MB，已跳过'); return; }
      var item = {
        file: file,
        name: file.name || '附件',
        size: file.size,
        mime: file.type || '',
        kind: guessKind(file),
        status: 'uploading',
        progress: 0,
        id: '',
        url: '',
        error: ''
      };
      state.uploads.push(item);
      renderUploadStrip();
      doUpload(item);
    });
  }

  function openFilePicker() {
    var input = $('file-input');
    if (!input) return;
    input.value = '';
    input.click();
  }

  function updateSendEnabled() {
    var btn = $('send-btn');
    if (!btn) return;
    var input = $('composer-input');
    var hasText = !!(input && input.value.trim());
    var hasAttachment = state.uploads.some(function (item) { return item.status === 'done'; });
    var uploading = state.uploads.some(function (item) { return item.status === 'uploading'; });
    btn.disabled = !!state.sendBusy || (!hasText && !hasAttachment) || (uploading && !hasText);
  }

  /* ================= 复制：图标按钮（代码块 / 完整回复） ================= */

  var ICON_COPY = '<svg class="ic-copy" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"></rect><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"></path></svg>';
  var ICON_CHECK = '<svg class="ic-check" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"></path></svg>';

  /// 统一复制反馈：成功后图标短暂变勾（1.5s），失败回退手动复制/提示。
  function copyWithFeedback(text, btn, failMsg) {
    var value = String(text === undefined || text === null ? '' : text);
    if (!value || !btn) return;
    function done() {
      if (btn.dataset.copying) return;
      btn.dataset.copying = '1';
      var original = btn.innerHTML;
      btn.innerHTML = ICON_CHECK;
      btn.classList.add('copied');
      setTimeout(function () {
        btn.innerHTML = original;
        btn.classList.remove('copied');
        delete btn.dataset.copying;
      }, 1500);
    }
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(value).then(done).catch(function () { fallbackCopy(value, done, failMsg); });
    } else {
      fallbackCopy(value, done, failMsg);
    }
  }

  function copyCodeText(btn) {
    var block = btn.closest ? btn.closest('.code-block') : null;
    if (!block) return;
    var codeEl = block.querySelector('code');
    var text = codeEl ? codeEl.textContent : '';
    // 只复制纯代码原文（不含语言标签/行号/UI 文案）。
    copyWithFeedback(text, btn, '复制失败，请长按选择代码');
  }

  /// 助手消息底部右侧的「复制完整回复」图标按钮。
  function makeReplyCopyButton(getText) {
    var btn = el('button', 'msg-copy');
    btn.type = 'button';
    btn.title = '复制完整回复';
    btn.setAttribute('aria-label', '复制完整回复');
    btn.innerHTML = ICON_COPY;
    btn.addEventListener('click', function (ev) {
      if (ev) ev.stopPropagation();
      copyWithFeedback(String(getText() || ''), btn, '复制失败，请长按选择文本');
    });
    return btn;
  }

  function replyActions(getText) {
    var row = el('div', 'msg-actions');
    row.appendChild(makeReplyCopyButton(getText));
    return row;
  }

  function fallbackCopy(text, done, failMsg) {
    var ta = document.createElement('textarea');
    ta.value = text;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    document.body.appendChild(ta);
    ta.select();
    var ok = false;
    try { ok = document.execCommand('copy'); } catch (e) {}
    document.body.removeChild(ta);
    if (ok) done();
    else toast(failMsg || '复制失败，请长按选择文本');
  }

  /* ================= 截图附件 ================= */

  function mountScreenshots(container, text) {
    text = String(text || '');
    var re = /\[(?:screenshot|image):([^\]]+)\]/g;
    var m;
    var seen = {};
    while ((m = re.exec(text)) !== null) {
      var name = m[1].trim().split(/[\\/]/).pop();
      if (!name || !/^[A-Za-z0-9._-]+\.png$/i.test(name) || seen[name]) continue;
      seen[name] = true;
      loadShot(name).then(function (url) {
        var img = el('img', 'shot');
        img.src = url;
        img.alt = '截图';
        img.loading = 'lazy';
        container.appendChild(img);
      }).catch(function () {});
    }
  }

  function loadShot(name) {
    if (state.shotCache[name]) return Promise.resolve(state.shotCache[name]);
    return fetch('/api/screenshot/' + enc(name), {
      headers: { Authorization: 'Bearer ' + state.token },
      cache: 'no-store'
    }).then(function (res) {
      if (!res.ok) throw new Error('截图读取失败');
      return res.blob();
    }).then(function (blob) {
      var url = URL.createObjectURL(blob);
      state.shotCache[name] = url;
      return url;
    });
  }

  /* ================= 模式 chip / 审查队列 ================= */

  function renderMode() {
    var chip = $('mode-chip');
    var label = (state.modeLabel || '').trim();
    if (!label) { show(chip, false); return; }
    chip.textContent = '模式：' + label;
    chip.setAttribute('data-mode', state.mode || '');
    chip.title = '当前工作模式：' + label;
    show(chip, true);
  }

  function renderProviderStatus(status) {
    var ball = $('provider-ball');
    if (!ball) return;
    state.providerStatus = status || null;
    var indicator = status && status.indicator ? String(status.indicator).toLowerCase() : '';
    if (!status || !indicator || indicator === 'unknown') {
      show(ball, false);
      ball.className = 'provider-ball hidden';
      // 后台预热状态可能还没就绪：4 秒后补查一次（只一次，避免频繁请求）。
      if (!state.statusRetryTimer && state.statusRetries < 6) {
        state.statusRetries += 1;
        state.statusRetryTimer = setTimeout(function () {
          state.statusRetryTimer = 0;
          if (state.token && !state.providerStatus) loadMe();
        }, 4000);
      }
      return;
    }
    ball.className = 'provider-ball ind-' + indicator;
    var title = (status.provider || 'API') + ' · ' + (status.label || '');
    if (status.detail) title += '（' + status.detail + '）';
    title += '：点击打开厂商状态页';
    ball.setAttribute('title', title);
    ball.setAttribute('aria-label', title);
    ball.href = status.url || '#';
    ball.setAttribute('target', '_blank');
    ball.setAttribute('rel', 'noopener noreferrer');
    state.statusRetries = 0;
    show(ball, true);
    var providerRow = $('srv-provider');
    if (providerRow) providerRow.textContent = core.providerStatusText(status);
  }

  function riskLabel(level) {
    var key = String(level || '').trim().toLowerCase();
    return RISK_LABELS[key] || RISK_LABELS[''];
  }

  function renderReview() {
    var bar = $('review-bar');
    var list = $('review-list');
    var items = state.review.items || [];
    if (!items.length) {
      show(bar, false);
      show(list, false);
      list.textContent = '';
      state.review.expanded = false;
      return;
    }
    var high = items.some(function (it) {
      return it.emphasized || it.risk_level === 'high' || it.risk_level === 'critical';
    });
    bar.classList.toggle('high', high);
    show(bar, true);
    $('review-text').textContent = items.length + ' 项操作等待桌面端审查（远程只能查看，不能批准）';
    var toggle = $('review-toggle');
    toggle.textContent = state.review.expanded ? '收起' : '查看';
    toggle.setAttribute('aria-expanded', state.review.expanded ? 'true' : 'false');
    show(list, state.review.expanded);
    if (!state.review.expanded) return;
    var frag = document.createDocumentFragment();
    items.forEach(function (it) {
      var row = el('div', 'review-item');
      var head = el('div', 'review-item-head');
      head.appendChild(el('span', 'review-item-tool', it.tool || '未知工具'));
      var level = el('span', 'review-level', riskLabel(it.risk_level));
      level.setAttribute('data-level', String(it.risk_level || '').toLowerCase());
      head.appendChild(level);
      row.appendChild(head);
      if (it.risk_reason) row.appendChild(el('div', '', it.risk_reason));
      if (it.args_summary) row.appendChild(el('div', 'review-args', it.args_summary));
      var meta = [];
      if (it.class) meta.push('类别：' + it.class);
      if (it.call_id) meta.push('call_id：' + truncate(it.call_id, 24));
      meta.push('需要在桌面端处理');
      row.appendChild(el('div', 'review-note', meta.join(' · ')));
      frag.appendChild(row);
    });
    list.textContent = '';
    list.appendChild(frag);
  }

  function mergeReviewItem(item) {
    if (!item || !item.call_id) return;
    var list = state.review.items || [];
    var idx = -1;
    for (var i = 0; i < list.length; i++) {
      if (list[i].call_id === item.call_id) { idx = i; break; }
    }
    if (idx >= 0) list[idx] = item;
    else list.push(item);
    state.review.items = list;
    renderReview();
  }

  function loadReviewPending() {
    return api('/api/review/pending').then(function (items) {
      state.review.items = Array.isArray(items) ? items : [];
      renderReview();
    }).catch(function () {});
  }

  function toggleReview() {
    state.review.expanded = !state.review.expanded;
    renderReview();
  }

  /* ================= WebSocket：心跳 + 退避重连 ================= */

  function stopHeartbeat() {
    if (state.hbTimer) {
      clearInterval(state.hbTimer);
      state.hbTimer = 0;
    }
  }

  function startHeartbeat() {
    stopHeartbeat();
    state.missedPongs = 0;
    state.hbTimer = setInterval(function () {
      var ws = state.ws;
      if (!ws || ws.readyState !== 1) return;
      try { ws.send(JSON.stringify({ type: 'ping' })); } catch (e) {}
      state.missedPongs += 1;
      if (core.shouldDeclareDisconnected(state.missedPongs)) {
        // 连续 3 次心跳无响应：判定连接已死，主动关闭走重连。
        stopHeartbeat();
        setConnState('offline');
        showConnBanner('连接已断开，正在重连…');
        try { ws.close(); } catch (e) {}
      }
    }, core.HEARTBEAT_INTERVAL_MS);
  }

  function connectWs() {
    if (!state.token) return;
    if (state.ws && (state.ws.readyState === 0 || state.ws.readyState === 1)) return;
    setConnState(state.wsAttempts > 0 ? 'reconnecting' : 'connecting');
    var proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    var query = '?token=' + enc(state.token);
    if (state.current) {
      query += '&conv_id=' + enc(state.current);
      var seq = currentLastSeq();
      if (seq > 0) query += '&since_seq=' + seq;
    }
    var url = proto + '//' + location.host + '/ws' + query;
    if (debugOfflineRequested()) {
      // 仅测试用：强制连一个不可解析的域名，用来验证重连提示。
      url = location.protocol === 'https:' ? 'wss://invalid.invalid:9/ws' : 'ws://invalid.invalid:9/ws';
    }
    var ws;
    try { ws = new WebSocket(url); } catch (e) { scheduleReconnect(); return; }
    state.ws = ws;
    var wasOffline = state.wsAttempts > 0;
    ws.onopen = function () {
      state.wsAttempts = 0;
      state.missedPongs = 0;
      state.wsConnections += 1;
      if (!perf.wsConnected) { perf.wsConnected = performance.now(); renderPerf(); }
      setConnState('online');
      startHeartbeat();
      loadReviewPending();
      reconnectRestore(wasOffline);
    };
    ws.onclose = function () {
      stopHeartbeat();
      if (state.ws === ws) state.ws = null;
      scheduleReconnect();
    };
    ws.onerror = function () {};
    ws.onmessage = function (ev) {
      state.missedPongs = 0;
      state.lastHeartbeatMs = Date.now();
      renderHeartbeat();
      var msg;
      try { msg = JSON.parse(ev.data); } catch (e) { return; }
      handleEvent(msg);
    };
  }

  function disconnectWs() {
    clearTimeout(state.reconnectTimer);
    stopHeartbeat();
    if (state.ws) { try { state.ws.close(); } catch (e) {} state.ws = null; }
    setConnState('offline');
  }

  function scheduleReconnect() {
    if (!state.token) return;
    state.wsAttempts += 1;
    var delay = core.nextReconnectDelay(state.wsAttempts);
    var showBanner = core.shouldShowDisconnectBanner(state.wsAttempts);
    if (showBanner) {
      setConnState('offline');
      showConnBanner('连接已断开，正在重连…（第 ' + state.wsAttempts + ' 次，约 ' + Math.round(delay / 1000) + ' 秒后重试）');
    } else {
      // 短暂断开：静默重试，不吓用户。
      setConnState('reconnecting');
    }
    clearTimeout(state.reconnectTimer);
    state.reconnectTimer = setTimeout(function () { connectWs(); }, delay);
    // 重试多次仍连不上时验证 token 是否已失效（401 会自动回配对页）。
    if (state.wsAttempts === 3) {
      api('/api/me').catch(function () {});
    }
  }

  function reconnectRestore(wasOffline) {
    loadMe();
    loadReviewPending();
    if (state.view === 'list') loadSessions(false);
    if (state.view === 'chat' && state.current) refreshCurrentConversation();
    if (wasOffline) toast('连接已恢复');
  }

  /* ================= 事件处理 ================= */

  function handleEvent(ev) {
    if (!ev || !ev.type) return;
    var d = ev.data;
    switch (ev.type) {
      case 'hello':
        if (d) {
          if (d.device) { state.device = d.device; saveDevice(d.device); }
          if (d.server) state.server = d.server;
          if (Array.isArray(d.pending_reviews)) { state.review.items = d.pending_reviews; renderReview(); }
          if (d.resume && d.resume.conv_id && d.resume.conv_id === state.current) {
            // 服务端确认了重连上下文；前端重新拉一次当前会话，补上断线期间的消息。
            refreshCurrentConversation();
          }
        }
        setConnState('online');
        loadMe();
        break;
      case 'turn_started': beginTurn(d && d.user_text); break;
      case 'phase':
        if (isChatVisible()) setStatus(String(d || ''));
        break;
      case 'reasoning_delta':
        if (isChatVisible()) appendReasoning(String(d || ''));
        break;
      case 'text_delta':
        if (isChatVisible()) appendText(String(d || ''));
        break;
      case 'assistant_message':
        if (isChatVisible()) finalizeAssistant(d && d.text, d && d.reasoning);
        scheduleSessionRefresh();
        loadReviewPending();
        break;
      case 'tool_started': addToolStarted(d || {}); break;
      case 'tool_output': appendToolOutput(d || {}); break;
      case 'tool_finished': finishTool(d || {}); break;
      case 'conversation_started':
        if (d && d.conversation_id) {
          state.current = d.conversation_id;
          state.currentTitle = d.title || state.currentTitle;
          if (isChatVisible()) $('top-title').textContent = state.currentTitle || '对话';
        }
        scheduleSessionRefresh();
        break;
      case 'images_attached':
        if (isChatVisible() && d) addNotice('notice', '已把 ' + (d.count || 1) + ' 张截图发给模型（来源：' + (d.source || '截图') + '）。');
        break;
      case 'subagent_started':
        if (isChatVisible() && d) addNotice('notice', '子代理启动：' + (d.label || d.id || ''));
        break;
      case 'subagent_finished':
        if (isChatVisible() && d) addNotice('notice', '子代理结束（' + (d.label || '') + '）：' + truncate(d.summary || '', 160));
        break;
      case 'approval_requested':
        if (isChatVisible()) addNotice('warn', '危险操作等待确认：' + ((d && d.name) || '') + '，请在桌面端处理。');
        else toast('有操作需要在桌面端确认');
        loadReviewPending();
        break;
      case 'risk_review':
        if (d) {
          mergeReviewItem({
            call_id: d.call_id || '',
            tool: d.tool || '',
            args_summary: '',
            risk_level: d.risk_level || '',
            risk_reason: d.risk_reason || '',
            class: '',
            emphasized: !!d.emphasized
          });
          toast('风险审查：' + (d.tool || '操作') + '（' + riskLabel(d.risk_level) + '）' +
            (d.risk_reason ? ' — ' + d.risk_reason : '') + '，请在桌面端处理');
        }
        loadReviewPending();
        break;
      case 'error': addNotice('error', String(d || '未知错误')); break;
      case 'notice': addNotice('notice', String(d || '')); break;
      case 'turn_finished':
        endTurn(!!(d && d.cancelled));
        // 桌面端一轮结束后，远程强制拉一次最新消息/忙闲状态（不依赖 delta 是否收全）。
        refreshCurrentConversation();
        loadMe();
        break;
      case 'review_resolved':
        // 桌面端已处理某个待确认操作：清对应横幅条目 + 拉最新消息（执行结果）。
        if (d && d.call_id) {
          state.review.items = (state.review.items || []).filter(function (it) {
            return it.call_id !== d.call_id;
          });
          renderReview();
        }
        loadReviewPending();
        refreshCurrentConversation();
        loadMe();
        break;
      case 'review_cleared':
        state.review.items = [];
        renderReview();
        loadReviewPending();
        refreshCurrentConversation();
        loadMe();
        break;
      default: break;
    }
  }

  /* ================= 设备信息 ================= */

  function loadMe() {
    return api('/api/me').then(function (data) {
      if (!data) return;
      if (data.device) { state.device = data.device; saveDevice(data.device); }
      if (data.server) {
        state.server = data.server;
        state.mode = data.server.mode || '';
        state.modeLabel = data.server.mode_label || '';
        renderMode();
        renderProviderStatus(data.server.provider_status || null);
      }
      renderDeviceInfo();
      return data;
    }).catch(function () {});
  }

  function loadDevices() {
    return api('/api/devices').then(function (list) {
      renderDeviceList(Array.isArray(list) ? list : []);
    }).catch(function (err) { toast('读取设备列表失败：' + err.message); });
  }

  function renderDeviceInfo() {
    var dev = state.device || {};
    var srv = state.server || {};
    $('dev-name').textContent = dev.name || '未命名设备';
    $('dev-id').textContent = dev.id || '—';
    $('dev-seen').textContent = dev.last_seen_ms ? fmtRelative(dev.last_seen_ms) : '—';
    $('dev-addr').textContent = dev.last_addr || '—';
    // v0.6 排障字段：服务绑在哪、手机该访问哪个地址、这次请求来自哪个 IP。
    $('srv-bind').textContent = srv.bind || '—';
    $('srv-advertise').textContent = srv.advertise_url || '—';
    $('dev-client').textContent = srv.client_ip || '—';
    renderHeartbeat();
    renderDevConn();
    $('srv-host').textContent = srv.hostname || '—';
    $('srv-version').textContent = srv.version || '—';
    $('srv-model').textContent = srv.model || '—';
    $('srv-mode').textContent = (srv.mode_label || state.modeLabel || '—');
    $('srv-busy').textContent = srv.busy ? '任务执行中' : '空闲';
    $('srv-lan').textContent = srv.lan_url || srv.lan_ip || '—';
    $('srv-pairing').textContent = srv.pairing_active
      ? '有效（可在桌面端刷新）'
      : '已过期或已使用，请在桌面端重新生成配对码';
    $('srv-review').textContent = (typeof srv.review_pending === 'number' ? srv.review_pending : (state.review.items || []).length) + ' 项';
    if ($('srv-provider')) {
      $('srv-provider').textContent = core.providerStatusText(state.providerStatus) || '未知（已隐藏小球）';
    }
    renderPerf();
  }

  function renderDeviceList(list) {
    var host = $('device-list');
    var frag = document.createDocumentFragment();
    if (!list.length) {
      frag.appendChild(el('div', 'device-row-meta', '还没有已信任设备。'));
    } else {
      var myId = (state.device && state.device.id) || '';
      list.forEach(function (d) {
        var row = el('div', 'device-row' + (d.id === myId ? ' current' : ''));
        row.appendChild(el('div', 'device-row-name', (d.name || d.id) + (d.id === myId ? '（本机）' : '')));
        row.appendChild(el('div', 'device-row-meta', (d.online ? '在线' : '离线') + ' · ' + (d.last_addr || '未知地址') + ' · ' + fmtRelative(d.last_seen_ms)));
        // v0.7：远程重命名（写回桌面端 devices.json，本地设置页也能看到）。
        var edit = el('div', 'device-row-edit');
        var input = el('input', 'device-name-input');
        input.type = 'text';
        input.maxLength = 32;
        input.value = d.name || '';
        input.setAttribute('aria-label', '设备名称');
        var save = el('button', 'btn btn-ghost', '重命名');
        save.type = 'button';
        save.addEventListener('click', function () { renameDevice(d, input.value, save); });
        edit.appendChild(input);
        edit.appendChild(save);
        row.appendChild(edit);
        frag.appendChild(row);
      });
    }
    host.textContent = '';
    host.appendChild(frag);
  }

  function renameDevice(device, newName, btn) {
    var name = String(newName || '').trim();
    if (!name) { toast('设备名不能为空'); return; }
    if (name === (device.name || '')) { toast('名字没有变化'); return; }
    btn.disabled = true;
    api('/api/devices/' + enc(device.id) + '/name', { method: 'POST', json: { name: name } })
      .then(function (updated) {
        device.name = (updated && updated.name) || name;
        if (state.device && state.device.id === device.id) {
          state.device.name = device.name;
          saveDevice(state.device);
        }
        toast('设备名已更新');
        renderDeviceInfo();
        loadDevices();
      })
      .catch(function (err) { toast('重命名失败：' + err.message); })
      .then(function () { btn.disabled = false; });
  }

  /* ================= 启动 ================= */

  function enterApp() {
    show($('boot'), false);
    show($('pair-view'), false);
    show($('app-view'), true);
    setView('list');
    show($('list-skeleton'), true);
    updateSendEnabled();
    renderUploadStrip();
    perf.appVisible = performance.now();
    loadSessions(true).then(function () {
      if (state.autoOpenedOnce) return;
      state.autoOpenedOnce = true;
      if (state.sessions.length) {
        openSession(state.sessions[0].id, state.sessions[0].title || '');
      } else {
        setView('chat');
        show($('chat-empty'), true);
        $('top-title').textContent = '新对话';
      }
    });
    loadMe();
    loadReviewPending();
    connectWs();
    if (!state.reviewPollTimer) {
      state.reviewPollTimer = setInterval(function () {
        if (!document.hidden && state.token) loadReviewPending();
      }, REVIEW_POLL_MS);
    }
    if (!state.statusTimer) {
      state.statusTimer = setInterval(function () {
        if (!document.hidden && state.token) loadMe();
      }, 60000);
    }
    if (!state.heartbeatTimer) {
      state.heartbeatTimer = setInterval(function () {
        if (state.view === 'device' && !document.hidden) renderHeartbeat();
      }, 5000);
    }
  }

  function enterAppOffline(err) {
    // 网络暂时不通：保留 token 进 app，显示重连横幅并后台重试，不把用户赶回配对页。
    enterApp();
    setConnState('offline');
    showConnBanner('暂时无法连接桌面端，正在自动重试…（' + (err && err.message ? err.message : '网络不通') + '）');
  }

  function autosize(textarea) {
    textarea.style.height = 'auto';
    textarea.style.height = Math.min(textarea.scrollHeight, 160) + 'px';
  }

  function bind() {
    $('pair-btn').addEventListener('click', manualPair);
    $('pair-code').addEventListener('input', function () {
      var start = this.selectionStart;
      var formatted = core.formatPairCode(this.value);
      if (formatted !== this.value) {
        this.value = formatted;
        try { this.setSelectionRange(start, start); } catch (e) {}
      }
    });
    $('pair-code').addEventListener('keydown', function (ev) { if (ev.key === 'Enter') { ev.preventDefault(); manualPair(); } });
    $('pair-name').addEventListener('keydown', function (ev) { if (ev.key === 'Enter') { ev.preventDefault(); manualPair(); } });

    $('back-btn').addEventListener('click', function () {
      if (isWideLayout()) {
        // 宽屏：左栏常驻，返回只把右栏切回对话。
        setView('chat');
        return;
      }
      setView('list');
      loadSessions(false);
    });
    $('device-btn').addEventListener('click', function () {
      if (state.view === 'device') { setView(state.current ? 'chat' : 'list'); return; }
      setView('device');
      loadMe();
      loadDevices();
      loadReviewPending();
    });
    $('new-btn').addEventListener('click', newConversation);
    $('send-btn').addEventListener('click', sendMessage);
    $('stop-btn').addEventListener('click', stopTurn);
    $('unpair-btn').addEventListener('click', unpair);
    $('refresh-devices-btn').addEventListener('click', function () { loadMe(); loadDevices(); loadReviewPending(); });
    $('jump-btn').addEventListener('click', jumpToBottom);
    $('review-toggle').addEventListener('click', toggleReview);
    // v0.7：附件上传（按钮 / 拖拽）+ 代码块复制 + 发送按钮可用态。
    var providerBall = $('provider-ball');
    if (providerBall) {
      providerBall.addEventListener('click', function (ev) {
        var ps = state.providerStatus;
        if (!ps || !ps.url) {
          ev.preventDefault();
          toast('暂时没有厂商状态页链接');
        }
      });
    }
    $('attach-btn').addEventListener('click', openFilePicker);
    $('file-input').addEventListener('change', function () {
      uploadFiles(this.files);
      this.value = '';
    });
    // v0.7.2：整个 app 区域支持拖拽上传（不只聊天列表），带可见浮层反馈。
    var dropZone = $('app-view');
    var dragDepth = 0;

    function dragHasFiles(ev) {
      var dt = ev.dataTransfer;
      if (!dt) return false;
      if (dt.files && dt.files.length) return true;
      if (dt.items) {
        for (var i = 0; i < dt.items.length; i++) {
          if (dt.items[i] && dt.items[i].kind === 'file') return true;
        }
      }
      return Array.prototype.indexOf.call(dt.types || [], 'Files') >= 0;
    }

    function dragFileCount(ev) {
      var dt = ev.dataTransfer;
      if (!dt) return 0;
      if (dt.files && dt.files.length) return dt.files.length;
      if (dt.items) {
        var n = 0;
        for (var i = 0; i < dt.items.length; i++) {
          if (dt.items[i] && dt.items[i].kind === 'file') n += 1;
        }
        return n;
      }
      return 1;
    }

    function collectDragFiles(dt) {
      var out = [];
      if (!dt) return out;
      if (dt.files && dt.files.length) {
        for (var i = 0; i < dt.files.length; i++) out.push(dt.files[i]);
        return out;
      }
      if (dt.items) {
        for (var j = 0; j < dt.items.length; j++) {
          var item = dt.items[j];
          if (item && item.kind === 'file') {
            var f = item.getAsFile ? item.getAsFile() : null;
            if (f) out.push(f);
          }
        }
      }
      return out;
    }

    function showDropOverlay(count) {
      var overlay = $('drop-overlay');
      if (!overlay) return;
      var n = $('drop-count');
      if (n) n.textContent = String(count || 1);
      show(overlay, true);
      if (dropZone) dropZone.classList.add('drag-over');
    }

    function hideDropOverlay() {
      dragDepth = 0;
      show($('drop-overlay'), false);
      if (dropZone) dropZone.classList.remove('drag-over');
    }

    ['dragenter', 'dragover'].forEach(function (name) {
      dropZone.addEventListener(name, function (ev) {
        if (!dragHasFiles(ev)) return;
        // 必须 preventDefault，否则浏览器不会触发 drop（这是之前"拖放无效"的根因之一）。
        ev.preventDefault();
        if (ev.dataTransfer) ev.dataTransfer.dropEffect = 'copy';
        if (name === 'dragenter') dragDepth += 1;
        if (dragDepth <= 1) dragDepth = 1;
        showDropOverlay(dragFileCount(ev));
      });
    });
    dropZone.addEventListener('dragleave', function (ev) {
      if (!dragHasFiles(ev)) return;
      dragDepth -= 1;
      if (dragDepth <= 0) hideDropOverlay();
    });
    dropZone.addEventListener('drop', function (ev) {
      var files = collectDragFiles(ev.dataTransfer);
      if (!files.length) return;
      ev.preventDefault();
      hideDropOverlay();
      uploadFiles(files);
    });
    // 兜底：拖到 app 外面时阻止浏览器直接打开/下载文件（不拦截页面内正常拖选）。
    document.addEventListener('dragover', function (ev) {
      if (dragHasFiles(ev)) ev.preventDefault();
    });
    document.addEventListener('drop', function (ev) {
      if (dragHasFiles(ev)) ev.preventDefault();
    });
    // v0.7.2：手机锁屏/切后台回来后，主动补一次状态与消息（resume/心跳之外的兜底）。
    // 宽屏断点切换（桌面窗口缩放 / 平板旋转）：重新排布视图，避免残留单栏/双栏状态。
    var wideMedia = window.matchMedia('(min-width: 900px), (orientation: landscape) and (min-width: 768px)');
    function onWideChange() {
      if (!state.token) return;
      setView(state.view);
      if (state.view === 'chat') refreshCurrentConversation();
    }
    if (wideMedia.addEventListener) wideMedia.addEventListener('change', onWideChange);
    else if (wideMedia.addListener) wideMedia.addListener(onWideChange);
    document.addEventListener('visibilitychange', function () {
      if (document.hidden || !state.token) return;
      loadMe();
      loadReviewPending();
      loadSessions(false);
      if (isChatVisible()) refreshCurrentConversation();
      if (!state.ws || state.ws.readyState > 1) connectWs();
    });
    messagesEl.addEventListener('click', function (ev) {
      var target = ev.target;
      if (!target || !target.closest) return;
      var copyBtn = target.closest('.code-copy');
      if (copyBtn) copyCodeText(copyBtn);
    });
    $('conn-retry').addEventListener('click', function () {
      clearTimeout(state.reconnectTimer);
      state.wsAttempts = 0;
      state.missedPongs = 0;
      hideConnBanner();
      connectWs();
    });

    var input = $('composer-input');
    input.addEventListener('input', function () { autosize(input); updateSendEnabled(); });
    input.addEventListener('keydown', function (ev) {
      if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); sendMessage(); }
    });

    messagesEl.addEventListener('scroll', onMessagesScroll, { passive: true });

    var search = $('search-input');
    search.addEventListener('input', function () {
      var q = search.value.trim();
      clearTimeout(state.searchTimer);
      state.searchTimer = setTimeout(function () { runSearch(q); }, 320);
    });

    // 宽屏断点切换（桌面窗口缩放 / 平板旋转）：重新排布视图，避免残留单栏/双栏状态。
    var wideMedia = window.matchMedia('(min-width: 900px), (orientation: landscape) and (min-width: 768px)');
    function onWideChange() {
      if (!state.token) return;
      setView(state.view);
      if (state.view === 'chat') refreshCurrentConversation();
    }
    if (wideMedia.addEventListener) wideMedia.addEventListener('change', onWideChange);
    else if (wideMedia.addListener) wideMedia.addListener(onWideChange);
    document.addEventListener('visibilitychange', function () {
      if (!document.hidden) {
        if (state.token && (!state.ws || state.ws.readyState > 1)) {
          state.wsAttempts = 0;
          connectWs();
        }
        if (state.view === 'list') loadSessions(false);
        loadReviewPending();
      }
    });

    window.addEventListener('online', function () {
      if (state.token) { state.wsAttempts = 0; connectWs(); }
    });
  }

  function runSearch(q) {
    var panel = $('search-panel');
    if (!q) {
      show(panel, false);
      panel.textContent = '';
      loadSessions(false);
      return;
    }
    api('/api/search?q=' + enc(q) + '&limit=30').then(function (hits) {
      hits = Array.isArray(hits) ? hits : [];
      panel.textContent = '';
      var frag = document.createDocumentFragment();
      if (!hits.length) {
        frag.appendChild(el('div', 'search-hit-snippet', '没有找到包含「' + q + '」的消息。'));
      } else {
        hits.forEach(function (h) {
          var btn = el('button', 'search-hit');
          btn.type = 'button';
          btn.appendChild(el('div', 'search-hit-title', h.conversation_title || '未命名对话'));
          btn.appendChild(el('div', 'search-hit-snippet', h.snippet || ''));
          btn.addEventListener('click', function () { show(panel, false); openSession(h.conversation_id, h.conversation_title || ''); });
          frag.appendChild(btn);
        });
      }
      panel.appendChild(frag);
      show(panel, true);
    }).catch(function (err) { toast('搜索失败：' + err.message); });
  }

  function boot() {
    messagesEl = $('messages');
    recordFirstPaint();
    state.token = loadToken();
    state.device = loadDevice();
    bind();

    var hash = '';
    try { hash = decodeURIComponent(String(location.hash || '').replace(/^#/, '')).trim(); } catch (e) { hash = ''; }
    var code = core.normalizePairCode(hash);
    var pairPath = location.pathname === '/pair';
    if (code) $('pair-code').value = core.formatPairCode(code);
    // v0.6：配对页显示"当前访问地址"，并在用 127.0.0.1/localhost 访问时警告手机连不上。
    if ($('pair-origin')) $('pair-origin').textContent = location.origin;
    var hostLower = String(location.hostname || '').toLowerCase();
    var isLoopback = hostLower === '127.0.0.1' || hostLower === 'localhost' || hostLower === '::1' || hostLower === '[::1]';
    show($('pair-loopback-warn'), isLoopback);

    if (state.token) {
      // 已有 token：先验证有效性，有效直接进 app（即使当前在 /pair 页面）。
      $('boot').textContent = '正在验证设备授权…';
      show($('boot'), true);
      api('/api/me', { skipAuthRedirect: true }).then(function (data) {
        if (data && data.device) { state.device = data.device; saveDevice(data.device); }
        enterApp();
      }).catch(function (err) {
        var status = err && typeof err.status === 'number' ? err.status : 0;
        if (status === 401) {
          clearAuth();
          if (code) {
            showPairView('设备授权已失效，请在桌面端查看新配对码。');
            state.autoOpenedOnce = false;
            show($('pair-auto'), false);
            $('pair-name').value = DEFAULT_DEVICE_NAME;
            autoPair(code);
          } else {
            showPairView('设备授权已失效，请在桌面端查看新配对码。');
            state.autoPairStarted = false;
          }
        } else if (code) {
          // 网络问题但带了新配对码：先尝试直接配对。
          showPairView('');
          $('pair-name').value = DEFAULT_DEVICE_NAME;
          autoPair(code);
        } else {
          enterAppOffline(err);
        }
      });
      return;
    }

    if (code) {
      // 扫码进入：自动填码 + 自动配对。
      showPairView('');
      $('pair-name').value = DEFAULT_DEVICE_NAME;
      autoPair(code);
      return;
    }
    showPairView('');
  }

  // 仅测试用：真机验证脚本用它模拟断线（设置 sessionStorage 标记并断开当前 WS）。
  window.__neosoniaDebug = {
    simulateOffline: function () {
      try { sessionStorage.setItem('neosonia.debug.offline', '1'); } catch (e) {}
      if (state.ws) { try { state.ws.close(); } catch (e) {} }
    },
    clearOffline: function () {
      try { sessionStorage.removeItem('neosonia.debug.offline'); } catch (e) {}
    },
    perf: function () { return window.__neosoniaPerf; }
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
















