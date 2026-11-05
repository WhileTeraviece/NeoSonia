/* NeoSonia 远程 SPA：Markdown / 表格 / 数学公式渲染核心（v0.7）
 *
 * 纯字符串处理、不碰 DOM，可被 Node 单测（scripts/test-spa-markdown.cjs）。
 *
 * 支持：
 *  - GFM 表格（表头 + 对齐 :--- / :---: / ---:，单元格内行内格式）；
 *  - 代码块（语言标注 / 复制按钮 / 横向滚动容器，不换行）；
 *  - 数学：$...$、$$...$$、\begin{aligned|cases|pmatrix|bmatrix|matrix|array}...\end{...}；
 *    轻量渲染（无外部字体/无 CDN）：\frac \sqrt 上下标 希腊字母 常用运算符 \text \left \right；
 *  - 附件标记解析（服务端在 prompt 里追加的【远程上传附件】段 → 附件卡）。
 */
(function (root, factory) {
  var api = factory();
  if (typeof module === 'object' && module.exports) module.exports = api;
  if (root) root.NeoSoniaMd = api;
})(typeof window !== 'undefined' ? window : null, function () {
  'use strict';

  function esc(s) {
    return String(s === undefined || s === null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }

  var GREEK = {
    alpha: 'α', beta: 'β', gamma: 'γ', delta: 'δ', epsilon: 'ε', zeta: 'ζ', eta: 'η',
    theta: 'θ', iota: 'ι', kappa: 'κ', lambda: 'λ', mu: 'μ', nu: 'ν', xi: 'ξ',
    pi: 'π', rho: 'ρ', sigma: 'σ', tau: 'τ', upsilon: 'υ', phi: 'φ', chi: 'χ',
    psi: 'ψ', omega: 'ω',
    Gamma: 'Γ', Delta: 'Δ', Theta: 'Θ', Lambda: 'Λ', Xi: 'Ξ', Pi: 'Π', Sigma: 'Σ',
    Upsilon: 'Υ', Phi: 'Φ', Psi: 'Ψ', Omega: 'Ω'
  };

  var SYMBOLS = {
    times: '×', cdot: '·', pm: '±', mp: '∓', div: '÷', ast: '∗',
    le: '≤', leq: '≤', ge: '≥', geq: '≥', ne: '≠', neq: '≠', approx: '≈', equiv: '≡',
    infty: '∞', to: '→', rightarrow: '→', leftarrow: '←', Rightarrow: '⇒',
    in: '∈', notin: '∉', forall: '∀', exists: '∃', partial: '∂', nabla: '∇',
    sum: '∑', prod: '∏', int: '∫', oint: '∮', sqrt: '√',
    ldots: '…', cdots: '⋯', dots: '…', quad: ' ', qquad: '  ',
    left: '', right: '', '{': '{', '}': '}', '%': '%', '#': '#', '&': '&',
    sin: 'sin', cos: 'cos', tan: 'tan', log: 'log', ln: 'ln', exp: 'exp', lim: 'lim'
  };

  /* ==================== 数学渲染 ==================== */

  function readGroup(src, i) {
    // src[i] 应该是 '{'；返回 {content, next}。
    var depth = 0;
    var start = i;
    for (; i < src.length; i++) {
      var ch = src[i];
      if (ch === '\\') { i++; continue; }
      if (ch === '{') depth++;
      else if (ch === '}') {
        depth--;
        if (depth === 0) {
          return { content: src.slice(start + 1, i), next: i + 1 };
        }
      }
    }
    return { content: src.slice(start + 1), next: src.length };
  }

  function readCommand(src, i) {
    // src[i] === '\\'
    var j = i + 1;
    var name = '';
    if (j < src.length && /[a-zA-Z]/.test(src[j])) {
      while (j < src.length && /[a-zA-Z]/.test(src[j])) { name += src[j]; j++; }
    } else if (j < src.length) {
      name = src[j];
      j++;
    }
    return { name: name, next: j };
  }

  function applyScript(atom, kind, content) {
    var inner = renderMathInline(content);
    if (kind === '^') return '<span class="m-sup-wrap">' + atom + '<span class="m-sup">' + inner + '</span></span>';
    return '<span class="m-sub-wrap">' + atom + '<span class="m-sub">' + inner + '</span></span>';
  }

  function renderMathInline(src) {
    src = String(src || '');
    var out = [];
    var i = 0;
    var lastAtomIndex = -1;
    while (i < src.length) {
      var ch = src[i];
      if (ch === '\\') {
        var cmd = readCommand(src, i);
        i = cmd.next;
        if (cmd.name === 'frac') {
          var a = readGroup(src, i); i = a.next;
          var b = readGroup(src, i); i = b.next;
          out.push('<span class="m-frac"><span class="m-num">' + renderMathInline(a.content) + '</span><span class="m-den">' + renderMathInline(b.content) + '</span></span>');
          lastAtomIndex = out.length - 1;
        } else if (cmd.name === 'sqrt') {
          var rad = readGroup(src, i); i = rad.next;
          out.push('<span class="m-sqrt"><span class="m-radical">√</span><span class="m-sqrt-body">' + renderMathInline(rad.content) + '</span></span>');
          lastAtomIndex = out.length - 1;
        } else if (cmd.name === 'text' || cmd.name === 'mathrm' || cmd.name === 'operatorname') {
          var t = readGroup(src, i); i = t.next;
          out.push('<span class="m-text">' + esc(t.content) + '</span>');
          lastAtomIndex = out.length - 1;
        } else if (cmd.name === 'left' || cmd.name === 'right') {
          // 丢弃 \left / \right，但紧随的括号字符会在下一轮被当普通字符。
          continue;
        } else if (GREEK[cmd.name] !== undefined) {
          out.push('<span class="m-t m-greek">' + esc(GREEK[cmd.name]) + '</span>');
          lastAtomIndex = out.length - 1;
        } else if (SYMBOLS[cmd.name] !== undefined) {
          out.push('<span class="m-t m-op">' + esc(SYMBOLS[cmd.name]) + '</span>');
          lastAtomIndex = out.length - 1;
        } else {
          out.push('<span class="m-raw">\\' + esc(cmd.name) + '</span>');
          lastAtomIndex = out.length - 1;
        }
        continue;
      }
      if (ch === '^' || ch === '_') {
        var g = readGroup(src, i + 1);
        i = g.next;
        if (lastAtomIndex >= 0) {
          out[lastAtomIndex] = applyScript(out[lastAtomIndex], ch, g.content);
        } else {
          out.push(applyScript('', ch, g.content));
          lastAtomIndex = out.length - 1;
        }
        continue;
      }
      if (ch === '{') {
        var grp = readGroup(src, i); i = grp.next;
        out.push(renderMathInline(grp.content));
        lastAtomIndex = out.length - 1;
        continue;
      }
      if (ch === '}') { i++; continue; }
      if (ch === '&') {
        out.push('<span class="m-t m-space"></span>');
        i++;
        lastAtomIndex = out.length - 1;
        continue;
      }
      if (ch === '~') { out.push('<span class="m-t m-space"></span>'); i++; lastAtomIndex = out.length - 1; continue; }
      // 普通字符成段输出（含中文/空格）。
      var run = '';
      while (i < src.length && !/[\\^_{}&~]/.test(src[i])) { run += src[i]; i++; }
      if (run) {
        out.push('<span class="m-t">' + esc(run) + '</span>');
        lastAtomIndex = out.length - 1;
      }
    }
    return out.join('');
  }

  var ENV_RE = /^\\begin\{([a-zA-Z*]+)\}(?:\{[^}]*\})?([\s\S]*)\\end\{\1\}$/;

  function renderMathEnv(name, body) {
    var rows = body.split(/\\\\/);
    var isMatrix = /matrix|array/.test(name);
    if (isMatrix) {
      var html = '<table class="m-matrix">';
      rows.forEach(function (row) {
        row = row.replace(/^\s*\n/, '').trim();
        if (!row) return;
        html += '<tr>';
        row.split('&').forEach(function (cell) {
          html += '<td>' + renderMathInline(cell.trim()) + '</td>';
        });
        html += '</tr>';
      });
      html += '</table>';
      return '<span class="m-matrix-wrap">' + html + '</span>';
    }
    var rowsHtml = '';
    rows.forEach(function (row) {
      row = row.replace(/^\s*\n/, '').trim();
      if (!row) return;
      var cells = row.split('&').map(function (c) { return renderMathInline(c.trim()); });
      rowsHtml += '<span class="m-case-row">' + cells.join('<span class="m-align-gap"></span>') + '</span>';
    });
    if (name === 'cases') {
      return '<span class="m-cases"><span class="m-case-brace">{</span><span class="m-case-body">' + rowsHtml + '</span></span>';
    }
    return '<span class="m-aligned">' + rowsHtml + '</span>';
  }

  function renderMath(latex, display) {
    var src = String(latex || '').trim();
    var html;
    try {
      var env = src.match(ENV_RE);
      if (env) {
        html = renderMathEnv(env[1], env[2]);
      } else {
        html = renderMathInline(src);
      }
    } catch (e) {
      html = '<code class="m-raw">' + esc(src) + '</code>';
    }
    if (display) return '<div class="math-block">' + html + '</div>';
    return '<span class="math">' + html + '</span>';
  }

  /* ==================== Markdown 主渲染 ==================== */

  function renderMarkdown(src) {
    var text = String(src === undefined || src === null ? '' : src).replace(/\r\n/g, '\n');
    var tokens = [];

    function stash(kind, body) {
      tokens.push({ kind: kind, body: body });
      return '\u0001' + (tokens.length - 1) + '\u0001';
    }
    function tokenIndex(placeholder) {
      var m = placeholder.match(/^\u0001(\d+)\u0001$/);
      return m ? parseInt(m[1], 10) : -1;
    }

    // 1) 围栏代码块（保留语言名，渲染时显示在头部）。
    text = text.replace(/```([A-Za-z0-9_+#.\-]*)[ \t]*\r?\n?([\s\S]*?)```/g, function (m, lang, body) {
      return '\n' + stash('code', { lang: (lang || '').trim(), code: body.replace(/\n$/, '') }) + '\n';
    });
    // 2) 块级公式 $$...$$ 与裸 \begin{...}...\end{...}。
    text = text.replace(/\$\$([\s\S]+?)\$\$/g, function (m, body) {
      return '\n' + stash('mathblock', body) + '\n';
    });
    text = text.replace(/\\begin\{([a-zA-Z*]+)\}[\s\S]*?\\end\{\1\}/g, function (m) {
      return '\n' + stash('mathblock', m) + '\n';
    });
    // 3) 行内公式 $...$（要求非空、不跨行；避免把 "$5" 之类误伤）。
    text = text.replace(/\$([^\s$][^$\n]*?[^\s$])\$/g, function (m, body) {
      return stash('mathinline', body);
    });
    // 4) 行内代码 `...`。
    text = text.replace(/`([^`\n]+)`/g, function (m, body) {
      return stash('icode', body);
    });

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
    function isBlockToken(line) {
      var idx = tokenIndex(line.trim());
      if (idx < 0) return false;
      var kind = tokens[idx].kind;
      return kind === 'code' || kind === 'mathblock';
    }
    function blockTokenHtml(line) {
      var idx = tokenIndex(line.trim());
      var t = tokens[idx];
      if (t.kind === 'code') {
        var lang = t.body.lang || '文本';
        return '<div class="code-block">' +
          '<div class="code-head"><span class="code-lang">' + esc(lang) + '</span>' +
          '<button class="code-copy" type="button">复制</button></div>' +
          '<pre><code>' + esc(t.body.code) + '</code></pre></div>';
      }
      return renderMath(t.body, true);
    }

    function splitTableRow(line) {
      var s = line.trim();
      if (s.startsWith('|')) s = s.slice(1);
      if (s.endsWith('|')) s = s.slice(0, -1);
      return s.split('|').map(function (c) { return c.trim(); });
    }
    function isTableSeparator(line) {
      return /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(line);
    }
    function renderTable(headerLine, sepLine, rowLines) {
      var head = splitTableRow(headerLine);
      var aligns = splitTableRow(sepLine).map(function (cell) {
        var left = cell.startsWith(':');
        var right = cell.endsWith(':');
        if (left && right) return 'center';
        if (right) return 'right';
        if (left) return 'left';
        return '';
      });
      var html = '<div class="table-wrap"><table><thead><tr>';
      head.forEach(function (cell, i) {
        html += '<th' + (aligns[i] ? ' class="ta-' + aligns[i] + '"' : '') + '>' + inlineMarkup(cell) + '</th>';
      });
      html += '</tr></thead><tbody>';
      rowLines.forEach(function (row) {
        var cells = splitTableRow(row);
        html += '<tr>';
        head.forEach(function (_, i) {
          var cell = cells[i] === undefined ? '' : cells[i];
          html += '<td' + (aligns[i] ? ' class="ta-' + aligns[i] + '"' : '') + '>' + inlineMarkup(cell) + '</td>';
        });
        html += '</tr>';
      });
      html += '</tbody></table></div>';
      return html;
    }

    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      var m;
      if (/^\s*$/.test(line)) { flushPara(); closeList(); continue; }
      if (isBlockToken(line)) {
        flushPara(); closeList();
        out.push(blockTokenHtml(line));
        continue;
      }
      // GFM 表格：当前行 + 下一行是分隔行。
      if (line.indexOf('|') >= 0 && i + 1 < lines.length && isTableSeparator(lines[i + 1])) {
        var rowLines = [];
        var j = i + 2;
        while (j < lines.length && lines[j].indexOf('|') >= 0 && !/^\s*$/.test(lines[j])) {
          rowLines.push(lines[j]);
          j++;
        }
        if (rowLines.length) {
          flushPara(); closeList();
          out.push(renderTable(line, lines[i + 1], rowLines));
          i = j - 1;
          continue;
        }
      }
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
      if (t.kind === 'icode') return '<code class="inline">' + esc(t.body) + '</code>';
      if (t.kind === 'mathinline') return renderMath(t.body, false);
      if (t.kind === 'mathblock') return renderMath(t.body, true);
      return '';
    });
  }

  // 链接渲染：file:/// 手机端只显示代码样式（不可点），http(s) 可点击。
  function linkHtml(label, url) {
    var safeUrl = String(url || '');
    var text = label || safeUrl;
    if (/^file:\/\//i.test(safeUrl)) {
      return '<code class="inline file-link" title="' + safeUrl + '">' + text + '</code>';
    }
    return '<a href="' + safeUrl + '" target="_blank" rel="noopener noreferrer">' + text + '</a>';
  }

  function inlineMarkup(s) {
    s = s.replace(/\*\*([^*\n]+?)\*\*/g, '<strong>$1</strong>');
    s = s.replace(/~~([^~\n]+?)~~/g, '<del>$1</del>');
    s = s.replace(/(^|[^*\w])\*([^*\n]+?)\*(?!\*)/g, '$1<em>$2</em>');
    // 已生成的链接先存成占位符，避免下面的裸 URL 规则二次链接。
    var stash = [];
    function put(html) {
      stash.push(html);
      return '\u0002' + (stash.length - 1) + '\u0002';
    }
    // 1) 标准 Markdown 链接：[文字](http(s)/file URL)
    s = s.replace(/\[([^\]\n]+)\]\(((?:https?|file):\/\/[^\s)]+)\)/g, function (all, label, url) {
      return put(linkHtml(label, url));
    });
    // 2) 原始 HTML 链接（已被转义）：<a href="...">文字</a>
    s = s.replace(/&lt;a\s+href=&quot;([\s\S]+?)&quot;[^&]*?&gt;([\s\S]*?)&lt;\/a&gt;/gi, function (all, url, label) {
      if (!/^(?:https?|file):\/\//i.test(url)) return all;
      return put(linkHtml(label, url));
    });
    // 3) 自动链接：<https://...> / <file:///...>
    s = s.replace(/&lt;((?:https?|file):\/\/[\s\S]+?)&gt;/g, function (all, url) {
      return put(linkHtml(url, url));
    });
    // 4) 裸 URL（http/https/file），保留结尾标点/括号。
    s = s.replace(/(^|[\s(（:：,，;；!！?？、=])((?:https?|file):\/\/[^\s<>()（）\u4e00-\u9fff，。；：！？、]+)/g, function (all, pre, url) {
      var trail = '';
      var m = url.match(/[.,;:!?，。；：！？、]+$/);
      if (m) {
        trail = m[0];
        url = url.slice(0, -m[0].length);
      }
      if (!url || url.length < 8) return all;
      return pre + put(linkHtml(url, url)) + trail;
    });
    s = s.replace(/\u0002(\d+)\u0002/g, function (all, idx) {
      return stash[parseInt(idx, 10)] || '';
    });
    return s;
  }

  /* ==================== 附件标记解析 ==================== */

  var ATTACH_MARKER = '【远程上传附件】';

  function splitAttachments(text) {
    var src = String(text === undefined || text === null ? '' : text);
    var idx = src.indexOf(ATTACH_MARKER);
    if (idx < 0) return { text: src.trim(), attachments: [] };
    var head = src.slice(0, idx).replace(/\s+$/, '').trim();
    var tail = src.slice(idx);
    var re = /^- (图片|文本|文件): (.+?)（(.+?)） (\/api\/uploads\/[0-9a-f]{32})/gm;
    var attachments = [];
    var m;
    while ((m = re.exec(tail)) !== null) {
      var kind = m[1] === '图片' ? 'image' : m[1] === '文本' ? 'text' : 'file';
      attachments.push({ kind: kind, name: m[2], size: m[3], url: m[4] });
    }
    return { text: head, attachments: attachments };
  }

  return {
    esc: esc,
    renderMarkdown: renderMarkdown,
    renderMath: renderMath,
    splitAttachments: splitAttachments
  };
});


