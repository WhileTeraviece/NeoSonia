// Markdown/表格/公式渲染纯逻辑单测（Node 运行，不需要 cargo）。
// 用法：node scripts/test-spa-markdown.cjs
// 覆盖：GFM 表格（对齐/单元格内行内格式）、轻量数学（$...$/$$...$$/aligned/cases/pmatrix）、
//       代码块（语言标注/复制按钮/转义/长行不换行）、附件标记解析、XSS 转义。

const assert = require('node:assert');
const md = require('../assets/web/md-core.js');

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

test('GFM 表格：表头/对齐/单元格内行内格式', () => {
  const src = [
    '| 名称 | 数量 | 备注 |',
    '| :--- | ---: | :---: |',
    '| **苹果** | 3 | `code` |',
    '| 香蕉 | 12 | 甜 |'
  ].join('\n');
  const html = md.renderMarkdown(src);
  assert.ok(html.includes('<table>'), html);
  assert.ok(html.includes('<thead>') && html.includes('<tbody>'), html);
  assert.ok(html.includes('ta-left') && html.includes('ta-right') && html.includes('ta-center'), html);
  assert.ok(html.includes('<strong>苹果</strong>'), html);
  assert.ok(html.includes('<code class="inline">code</code>'), html);
  assert.ok(!html.includes('| ---'), '不应残留分隔行原文');
  assert.ok(!html.includes('| 名称 |'), '不应残留表格原文');
});

test('GFM 表格：缺列/多列不崩', () => {
  const html = md.renderMarkdown('| a | b |\n| --- | --- |\n| 1 |\n| 1 | 2 | 3 |');
  assert.ok(html.includes('<table>'), html);
  assert.ok(html.includes('<td></td>'), '缺的单元格应补空');
  // 多出来的单元格按表头列数截断（不崩、不撑破布局）。
  assert.strictEqual((html.match(/<tr>/g) || []).length, 3, html);
  assert.ok(!html.includes('<td>3</td>'), '超出表头列数的单元格应被截断');
});

test('表格外的管道符仍按普通文本处理', () => {
  const html = md.renderMarkdown('a | b 只是文本，不是表格');
  assert.ok(!html.includes('<table>'), html);
  assert.ok(html.includes('a | b'), html);
});

test('行内公式 $...$', () => {
  const html = md.renderMarkdown('能量公式 $E=mc^2$ 与 $\\frac{a}{b}$。');
  assert.ok(html.includes('class="math"'), html);
  assert.ok(html.includes('m-frac'), html);
  assert.ok(html.includes('m-sup'), html);
  assert.ok(!html.includes('$E'), '不应残留 $ 原文');
});

test('块级公式 $$...$$', () => {
  const html = md.renderMarkdown('前文\n\n$$\\frac{a+b}{c}$$\n\n后文');
  assert.ok(html.includes('class="math-block"'), html);
  assert.ok(html.includes('m-num') && html.includes('m-den'), html);
});

test('希腊字母/常用运算符/sqrt/text', () => {
  const html = md.renderMath('\\alpha \\times \\beta \\le \\infty \\sqrt{x} \\text{当} x>0', true);
  assert.ok(html.includes('α') && html.includes('×') && html.includes('β'), html);
  assert.ok(html.includes('≤') && html.includes('∞'), html);
  assert.ok(html.includes('m-sqrt'), html);
  assert.ok(html.includes('当'), html);
});

test('\\begin{aligned} 多行环境', () => {
  const html = md.renderMath('\\begin{aligned}a &= b + c \\\\ d &= e - f\\end{aligned}', true);
  assert.ok(html.includes('m-aligned'), html);
  assert.ok(html.includes('m-case-row'), html);
});

test('\\begin{cases}', () => {
  const html = md.renderMath('\\begin{cases}x & x>0 \\\\ -x & x\\le 0\\end{cases}', true);
  assert.ok(html.includes('m-cases'), html);
  assert.ok(html.includes('m-case-brace'), html);
});

test('\\begin{pmatrix}/bmatrix 矩阵', () => {
  const p = md.renderMath('\\begin{pmatrix}1 & 2 \\\\ 3 & 4\\end{pmatrix}', true);
  assert.ok(p.includes('m-matrix'), p);
  assert.strictEqual((p.match(/<td>/g) || []).length, 4, p);
  const b = md.renderMath('\\begin{bmatrix}a & b\\end{bmatrix}', true);
  assert.ok(b.includes('m-matrix'), b);
});

test('代码块：语言标注 + 复制按钮 + 转义 + 长行不换行', () => {
  const longLine = 'x = 1 + ' + '1 + '.repeat(80) + '1';
  const src = '```python\n' + longLine + '\nprint("<script>")\n```';
  const html = md.renderMarkdown(src);
  assert.ok(html.includes('class="code-lang"'), html);
  assert.ok(html.includes('>python<'), html);
  assert.ok(html.includes('class="code-copy"'), html);
  assert.ok(html.includes('title="复制代码"'), '复制按钮应有 tooltip');
  assert.ok(html.includes('aria-label="复制代码"'), '复制按钮应有 aria-label');
  assert.ok(!html.includes('>复制<'), '复制按钮应改为图标');
  assert.ok(html.includes('class="ic-copy"'), '复制按钮应含 SVG 图标');
  assert.ok(html.includes('&lt;script&gt;'), '代码内容必须转义');
  assert.ok(!html.includes('<script>'), '代码内容不能注入标签');
  assert.ok(html.includes(longLine.replace(/</g, '&lt;')), '长行应完整保留（由 CSS 横向滚动）');
});

test('代码块无语言时显示"文本"标注', () => {
  const html = md.renderMarkdown('```\nplain\n```');
  assert.ok(html.includes('>文本<'), html);
});

test('Markdown 标准链接：[文字](http/https)', () => {
  const html = md.renderMarkdown('看 [DeepSeek 官网](https://www.deepseek.com) 和 [文档](http://example.com/docs?x=1&y=2)。');
  assert.ok(html.includes('href="https://www.deepseek.com"'), html);
  assert.ok(html.includes('href="http://example.com/docs?x=1&amp;y=2"'), html);
  assert.ok(html.includes('>DeepSeek 官网</a>'), html);
  assert.ok(!html.includes('](https://'), '不应残留 Markdown 链接语法');
});

test('file:/// 链接只显示代码样式、不可点', () => {
  const html = md.renderMarkdown('打开 [日志](file:///C:/Users/example/NeoSonia/logs/web.log) 看看');
  assert.ok(html.includes('class="inline file-link"'), html);
  assert.ok(html.includes('>日志</code>'), html);
  assert.ok(!html.includes('<a href="file:///'), 'file 链接不应可点击');
  assert.ok(!html.includes('](file:///'), '不应残留 Markdown 语法');
});

test('裸 URL 自动链接（http/https/file）', () => {
  const html = md.renderMarkdown('访问 https://status.deepseek.com 或 http://example.com/a 以及 file:///C:/tmp/x.md 即可。');
  assert.ok(html.includes('href="https://status.deepseek.com"'), html);
  assert.ok(html.includes('href="http://example.com/a"'), html);
  assert.ok(html.includes('class="inline file-link"'), html);
});

test('原始 HTML 链接 <a href> 被渲染成链接', () => {
  const html = md.renderMarkdown('链接：<a href="https://example.com/a?b=1&c=2">示例链接</a>');
  assert.ok(html.includes('href="https://example.com/a?b=1&amp;c=2"'), html);
  assert.ok(html.includes('>示例链接</a>'), html);
  assert.ok(!html.includes('&lt;a href'), '不应显示转义后的原始 HTML');
});

test('自动链接 <https://...> 与 <file:///...>', () => {
  const html = md.renderMarkdown('参见 <https://example.com/docs> 和 <file:///C:/tmp/a.md>');
  assert.ok(html.includes('href="https://example.com/docs"'), html);
  assert.ok(html.includes('class="inline file-link"'), html);
  assert.ok(!html.includes('&lt;https://'), '不应残留自动链接语法');
});

test('裸 URL 紧跟中文标点也能自动链接', () => {
  const html = md.renderMarkdown('链接：https://status.deepseek.com，文件：file:///C:/tmp/a.md。');
  assert.ok(html.includes('href="https://status.deepseek.com"'), html);
  assert.ok(html.includes('class="inline file-link"'), html);
});

test('有序列表：缩进/编号正确、长链接不溢出', () => {
  const longUrl = 'https://example.com/very/long/path/' + 'segment/'.repeat(20);
  const src = '步骤：\n\n1. 打开 [文档](https://example.com/docs)\n2. 访问 ' + longUrl + '\n3. 完成';
  const html = md.renderMarkdown(src);
  assert.ok(html.includes('<ol>'), html);
  assert.strictEqual((html.match(/<li>/g) || []).length, 3, html);
  assert.ok(html.includes('<a href="' + longUrl + '"'), html);
  assert.ok(!html.includes('1. 打开'), '列表编号不应残留原文');
  assert.ok(!html.includes('['), '不应残留 Markdown 链接语法');
});

test('XSS：用户文本转义', () => {
  const html = md.renderMarkdown('<img src=x onerror=alert(1)> & <b>raw</b>');
  assert.ok(!html.includes('<img src=x'), html);
  assert.ok(html.includes('&lt;img src=x'), html);
  assert.ok(html.includes('&amp;'), html);
});

test('长 URL / 长单词不丢内容（靠 CSS overflow-wrap）', () => {
  const long = 'https://example.com/' + 'a'.repeat(300);
  const html = md.renderMarkdown('看 ' + long);
  assert.ok(html.includes(long), html);
});

test('附件标记解析：图片/文本/文件', () => {
  const id1 = '0123456789abcdef0123456789abcdef';
  const id2 = 'fedcba9876543210fedcba9876543210';
  const id3 = '00112233445566778899aabbccddeeff';
  const text = '帮我看下这些\n\n【远程上传附件】\n- 图片: 截图.png（2.1 MB） /api/uploads/' + id1 +
    '\n- 文本: 笔记.md（3 KB） /api/uploads/' + id2 +
    '\n内容预览：\n```\nhi\n```\n- 文件: 包.zip（9 MB） 本地路径：/tmp/x.zip';
  const out = md.splitAttachments(text);
  assert.strictEqual(out.text, '帮我看下这些');
  assert.strictEqual(out.attachments.length, 2, '只有带 URL 的行会被解析成附件卡');
  assert.deepStrictEqual(out.attachments[0], {
    kind: 'image', name: '截图.png', size: '2.1 MB', url: '/api/uploads/' + id1
  });
  assert.strictEqual(out.attachments[1].kind, 'text');
  assert.strictEqual(out.attachments[1].name, '笔记.md');
});

test('无附件标记的普通消息原样返回', () => {
  const out = md.splitAttachments('普通消息，没有附件');
  assert.strictEqual(out.text, '普通消息，没有附件');
  assert.deepStrictEqual(out.attachments, []);
});

if (failures.length) {
  console.error('\n' + failures.length + ' 个失败：');
  failures.forEach((f) => console.error('  - ' + f));
  process.exit(1);
}
console.log('\nSPA_MARKDOWN_TESTS_PASS ' + passed);

