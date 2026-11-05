# NeoSonia · UI 设计规范（Firefox Nova 基调）

> 适用：常驻桌面右侧的长条浮窗 AI Agent（宽 400–440 px，高约屏幕 88%，置顶、圆角、有层次）。
> 目标：让渲染器（Rust / fenestra）可以逐项落地，而不是"看起来像"。
> 版本：1.0 · 2026-10-31 · 配套机器可读文件：`docs/ui-design-palette.json`

---

## 0. 事实基础（先对齐"什么是 Nova"）

Firefox 157（2026-09-29 全量推送）上线的界面改版，内部代号 **Project Nova**。它不是换皮，而是把 Firefox 的
设计系统（Acorn）重写了一遍：更圆的控件、更柔和的标签、火感的配色、重新平衡的间距，并用统一的 token /
组件 / 模式替换原来的零散样式。旧版 Proton 的"选中标签阴影"被移除，侧边栏与工具栏合并成同一块表面，
**垂直标签栏成为一等布局**。

本规范中所有色值、圆角、间距、阴影、缓动均来自 Mozilla 公开的真实产物，而非臆测：

| 类别 | 真实来源 |
| --- | --- |
| Nova 设计意图（"current, but not generic / warm, but precise"、标签柔化+微渐变、组件更圆、火感配色） | [Designing Firefox for the future](https://blog.mozilla.org/en/firefox/new-firefox-design/)（2026-05-21，Firefox UX Team） |
| Nova 上线与最终形态（圆角更大、地址栏更柔、选中标签微渐变、设置面板卡片化、紧凑模式回归） | [More modern. More flexible. Still Firefox.](https://blog.mozilla.org/en/firefox/new-firefox-design-is-here/)（2026-09-29）；[It's FOSS 上手对比](https://itsfoss.com/news/firefox-nova-redesign/) |
| 默认主题用 **Nova 紫**、选中标签去阴影、侧边栏与工具栏同表面、垂直标签、主题渐变 | [Nova is here: what changes for your Firefox theme](https://blog.mozilla.org/addons/2026/09/29/nova-is-here-what-changes-for-your-firefox-theme/)（Mozilla Add-ons Blog） |
| token 体系（base/application/component 三层、OKLCH、light/dark、16 px 间距基准） | [Acorn Design System · Design tokens](https://acorn.firefox.com/latest/desktop/design-tokens/how-design-tokens-work/overview-YYp6MVjt)；[Firefox Source Docs · Design Tokens](https://firefox-source-docs.mozilla.org/toolkit/themes/shared/design-system/docs/README.design-tokens.stories.html) |
| **Nova 实际色阶与语义 token（本文件色板的一手数据）** | mozilla-firefox/firefox：`color.nova.tokens.json`、`border.nova.tokens.json`、`box-shadow.nova.tokens.json`、`background.tokens.json`、`text.tokens.json`、`space.tokens.json`、`font.tokens.json`、`motion.tokens.json`、`browser/themes/shared/tabbrowser/tab.nova.tokens.json` |
| Nova 主题包的真实 hex（Lavender / Flame 的 frame、gradient、button hover 等） | [FirefoxUX/acorn-themes · themes/nova](https://github.com/FirefoxUX/acorn-themes/tree/main/themes/nova) |

结论先写在前面：**Nova 的强调色是紫（violet），不是橙，也不是蓝。**

| 候选 | Nova 中的真实角色 | 本产品是否用作主强调色 |
| --- | --- | --- |
| 紫 / violet `#764edd`(L) `#b89cff`(D) | `color.accent.primary`：按钮、链接、focus outline、选中态 | 是（唯一主强调色） |
| 橙 `#f4682c`(L) `#ff9565`(D) | 选中标签的 **trailing 发丝边**（`tab.border.color.selected.trailing`）、Nova Flame 主题 | 仅作 2 px 品牌渐变的一端，不做按钮/大面积 |
| 蓝 `#0062fa` / 青 `#00cadb` | 旧 Acorn v1 的 accent（桌面 Color 文档仍可见），Nova 已被 violet 取代 | 否，不再使用 |

---

## 1. 设计原则（5 条）

**1）这是"竖着的工具栏"，不是缩小版的网页。**
浮窗宽 400–440 px、贴右侧、常驻：横向空间是最稀缺资源，所以只允许单列纵向流。导航固定在顶部（不占宽度），
内容单列滚动，任何"左右分栏 / 侧边图标栏 / 多列卡片网格"都在这个宽度下直接失败。深度整合感来自
"像系统的一部分"的几何与节奏，而不是把 Windows 设置页照搬一遍。

**2）柔和，但精确（Nova 原话：warm, but precise）。**
圆角有明确档位、阴影只用于"脱离平面"的层级、渐变只保留 Nova 选中标签那条紫→橙发丝。
柔和靠**低饱和的紫灰中性色 + 8/12/16 的圆角节奏**，不靠大圆角 + 毛玻璃 + 彩色渐变堆出来。

**3）状态必须三重编码：色点 + 图标 + 文字。**
工具调用、连接状态、错误提示一律 `状态点(6 px) + Lucide 线性图标 + 中文短语`。颜色只做加速识别，
不作为唯一信息通道；任何时候截图去色后，界面仍然可读。**全产品零 emoji**。

**4）系统深度整合 = 同一套材质与习惯，而不是模仿系统控件。**
48 px 标题栏（整条可拖动）、Windows 11 的圆角/阴影观感、Segoe UI Variable 字体、系统 accent 可选跟随、
键盘优先（Ctrl+K 唤起、Ctrl+1/2/3 切页、Esc 关闭）。但控件外观是 Nova 的（圆角、发丝描边、紫强调），
不做 WinUI 复刻——复刻会立刻显得"像设置面板插件"，而不是 Copilot 级的系统能力。

**5）克制到"少一个像素都嫌多"。**
一屏最多 2 种强调色（紫 + 状态色）、最多 3 个字号层级同屏出现、卡片最多两层嵌套。
持续常驻的窗口，任何装饰都会在 8 小时后变成噪音。

**交叉印证**（同代桌面 AI / 工具浮窗的成熟做法）：

| 参考 | 做法 | 我们采纳的部分 |
| --- | --- | --- |
| Windows Copilot（Win11 桌面右侧停靠） | 贴边、系统级唤起、圆角浮层、与 Shell 同一视觉重量 | 右侧贴边 + 置顶 + 48 px 标题栏 + 单一主输入区 |
| [Raycast](https://github.com/sweetkey/awesome-design-md/blob/main/design-md/raycast/DESIGN.md) | 近黑而非纯黑底、多层阴影、6–12 px 圆角、hover 用透明度而非换色、键盘优先 | `#121114` 而非 `#000`；卡片 12 px；hover ≤90 ms；快捷键体系 |
| Arc | 侧边栏承载导航与空间切换 | 顶部主导航固定、切页不改变输入区位置 |
| Linear | 低饱和中性色 + 极少量强调色 + 12 px 卡片 | 强调色只出现在可交互/激活处，其余全部灰阶 |

---

## 2. 色彩系统

### 2.1 取色逻辑

Nova 的灰阶**带紫味**（`gray.90 #121114`、`gray.70 #252428`、`gray.05 #f7f6fb`），这是它和"通用灰"最大的区别；
本规范的中性色全部取自 Nova gray / violet-desaturated 阶，因此即使 0 个彩色元素，界面也仍然是 Nova。
文字色采用 Nova 的 `text.color`（light `violet-desaturated.90 #180e30` / dark `violet-desaturated.0 #f2f0f8`），
而不是纯黑纯白——这是"暖"的来源。

层级顺序（暗色）：`bg #121114` → `surface #171519` → `surface_raised #252428` → `overlay #312f33`，每级约 +4~5 L 点，
配合 1 px 发丝描边区分，不靠阴影。

### 2.2 Dark 色板

| Token | Dark hex | Dark OKLCH |
| --- | --- | --- |
| `bg` | `#121114` | `oklch(18.0% 0.006 300.9)` |
| `surface` | `#171519` | `oklch(20.0% 0.008 308.0)` |
| `surface_raised` | `#252428` | `oklch(26.3% 0.007 297.3)` |
| `overlay` | `#312f33` | `oklch(30.9% 0.008 308.2)` |
| `border_subtle` | `#2b292e` | `oklch(28.5% 0.009 303.9)` |
| `border_strong` | `#515054` | `oklch(43.4% 0.007 297.6)` |
| `text_primary` | `#f2f0f8` | `oklch(95.9% 0.011 297.6)` |
| `text_secondary` | `#aeacb3` | `oklch(74.8% 0.010 299.2)` |
| `text_tertiary` | `#807e84` | `oklch(59.7% 0.009 301.2)` |
| `accent` | `#b89cff` | `oklch(75.6% 0.141 295.9)` |
| `accent_hover` | `#d4c1ff` | `oklch(84.8% 0.087 298.6)` |
| `accent_pressed` | `#eaddff` | `oklch(91.8% 0.048 302.8)` |
| `on_accent` | `#1a1220` | `oklch(19.9% 0.030 310.0)` |
| `brand_from` | `#b89cff` | `oklch(75.6% 0.141 295.9)` |
| `brand_to` | `#ff9565` | `oklch(77.2% 0.143 44.5)` |
| `success` | `#4acca6` | `oklch(76.4% 0.127 170.1)` |
| `success_bg` | `#004933` | `oklch(35.9% 0.075 165.2)` |
| `warning` | `#f3a81e` | `oklch(78.5% 0.159 75.3)` |
| `warning_bg` | `#5f3100` | `oklch(36.6% 0.087 59.0)` |
| `danger` | `#ffb6bf` | `oklch(84.7% 0.086 10.9)` |
| `danger_bg` | `#69172d` | `oklch(35.1% 0.115 9.9)` |
| `selection` | `#423c53` | `oklch(37.2% 0.040 296.4)` |
| `focus_ring` | `#b89cff` | `oklch(75.6% 0.141 295.9)` |
| `scrollbar_thumb` | `#515054` | `oklch(43.4% 0.007 297.6)` |
| `scrollbar_track` | `#00000000` | `oklch(0.0% 0.000 0.0 / 0%)` |
| `scrim` | `#00000080` | `oklch(0.0% 0.000 0.0 / 50%)` |

### 2.3 Light 色板

| Token | Light hex | Light OKLCH |
| --- | --- | --- |
| `bg` | `#f7f6fb` | `oklch(97.5% 0.007 295.5)` |
| `surface` | `#fcfbff` | `oklch(99.0% 0.005 297.7)` |
| `surface_raised` | `#ffffff` | `oklch(100.0% 0.000 0.0)` |
| `overlay` | `#ffffff` | `oklch(100.0% 0.000 0.0)` |
| `border_subtle` | `#e3e2e7` | `oklch(91.5% 0.007 295.4)` |
| `border_strong` | `#b7b6ba` | `oklch(77.8% 0.006 297.7)` |
| `text_primary` | `#180e30` | `oklch(20.0% 0.065 293.2)` |
| `text_secondary` | `#5f5770` | `oklch(47.4% 0.041 299.5)` |
| `text_tertiary` | `#736d83` | `oklch(54.8% 0.034 297.7)` |
| `accent` | `#764edd` | `oklch(54.7% 0.207 290.8)` |
| `accent_hover` | `#5939a8` | `oklch(44.6% 0.169 291.3)` |
| `accent_pressed` | `#3e2976` | `oklch(35.2% 0.126 291.1)` |
| `on_accent` | `#ffffff` | `oklch(100.0% 0.000 0.0)` |
| `brand_from` | `#b89cff` | `oklch(75.6% 0.141 295.9)` |
| `brand_to` | `#ff9565` | `oklch(77.2% 0.143 44.5)` |
| `success` | `#06674b` | `oklch(45.6% 0.093 166.1)` |
| `success_bg` | `#c4f1e0` | `oklch(92.2% 0.052 170.2)` |
| `warning` | `#854800` | `oklch(46.6% 0.109 59.9)` |
| `warning_bg` | `#fde8b5` | `oklch(93.6% 0.069 88.3)` |
| `danger` | `#961e3d` | `oklch(44.6% 0.155 11.9)` |
| `danger_bg` | `#ffd9df` | `oklch(91.9% 0.043 7.2)` |
| `selection` | `#e4dcf8` | `oklch(90.9% 0.039 298.7)` |
| `focus_ring` | `#764edd` | `oklch(54.7% 0.207 290.8)` |
| `scrollbar_thumb` | `#c7c6cb` | `oklch(82.9% 0.007 295.4)` |
| `scrollbar_track` | `#00000000` | `oklch(0.0% 0.000 0.0 / 0%)` |
| `scrim` | `#00000080` | `oklch(0.0% 0.000 0.0 / 50%)` |

### 2.4 用法与对比度（实测 WCAG 比值）

| 组合 | Dark | Light | 结论 |
| --- | --- | --- | --- |
| `text_primary` on `bg` | 16.7:1 | 17.1:1 | 正文/标题，AAA |
| `text_primary` on `surface_raised` | 13.7:1 | 18.4:1 | 卡片正文，AAA |
| `text_secondary` on `surface_raised` | 6.9:1 | 6.8:1 | 次要说明，AA |
| `text_tertiary` on `bg` | 4.7:1 | 4.6:1 | 元信息/占位符，AA（12 px 以下不要用） |
| `accent` on `bg` | 8.3:1 | 5.0:1 | 链接/强调文字，AA |
| `on_accent` on `accent` | 8.0:1 | 5.4:1 | 主按钮文字，AA |
| `success` on `success_bg` | 5.2:1 | 5.6:1 | 成功横幅，AA |
| `warning` on `warning_bg` | 5.4:1 | 6.0:1 | 警告横幅，AA |
| `danger` on `danger_bg` | 7.2:1 | 6.4:1 | 错误横幅，AA |
| `border_strong` on `bg` | 2.4:1 | 1.9:1 | **仅装饰**，不承担可达性；焦点/边界由 `focus_ring` 保证 |

两条硬规则：
1. **文字不落在 `border_*` 之上**；`border_subtle` 只做分隔，`border_strong` 只做输入框静止态轮廓。
2. 深色下主按钮文字用 `on_accent #1a1220`，**不用** Nova 原 token 的 `gray.55`（实测对比仅 3.5:1，不达标）；
   这是本规范对 Nova 最明显的一处有意偏离。另外两处已写入 JSON：亮色三级文字用 60% 混合（Nova 只有 69%/40%
   两档，60% 才够 4.5:1）；三级文字本身是我们补的一档（暗 48% / 亮 60%），Nova 原始语义里没有。

### 2.5 渐变规则（只允许一种）

```
--brand-hairline: linear-gradient(90deg, #b89cff 0%, #ff9565 100%);  /* brand_from -> brand_to */
```

- 允许：激活导航项底部 2 px 下划线、助手头像 1.5 px 描边、面板顶部 1 px 高光（叠加 ≤12% 白）。
- 依据：Nova 选中标签的 leading 边是 `violet.30 #b89cff`、trailing 边是 `orange.30 #ff9565`，
  即"紫→橙"的横向发丝渐变——这是 Nova 唯一的大面积可见渐变语言（其主题包的大背景渐变属于主题，不属于控件）。
- 禁止：背景大面积渐变、卡片渐变、文字渐变、彩虹/多色渐变、任何蓝紫渐变（会立刻变成"AI 通用模板"）。

---

## 3. 排版

### 3.1 字体栈（Windows 11 优先）

```css
--font-ui: "Segoe UI Variable Text", "Segoe UI Variable", "Segoe UI", system-ui,
           "Microsoft YaHei UI", "Microsoft YaHei", "PingFang SC", "Noto Sans SC", sans-serif;
--font-display: "Segoe UI Variable Display", "Segoe UI Variable", "Segoe UI", system-ui,
           "Microsoft YaHei UI", "PingFang SC", sans-serif;
--font-mono: "Cascadia Code", "Cascadia Mono", "Consolas", "SF Mono", "JetBrains Mono",
           "Microsoft YaHei UI", monospace;
```

- `Segoe UI Variable Text` 用于 ≤16 px，`Display` 用于 ≥20 px（可变字体的光学尺寸差异在中文混排时尤其明显）。
- 中文回退 **Microsoft YaHei UI** 必须显式写死，不依赖 `system-ui`；不要 SimSun / 宋体。
- 等宽栈只用于：代码块、工具参数与输出、ID、耗时、token 计数、快捷键键帽。中文在等宽栈里回退到雅黑，允许。
- 数字一律开 `font-variant-numeric: tabular-nums`（耗时、计数、时间戳不跳动）。

### 3.2 字号阶梯（px / 字重 / 行高 / 字距 / 用途）

| Token | px | 字重 | 行高 | 字距 | 用途 |
| --- | --- | --- | --- | --- | --- |
| `caption` | 11 | 500 | 1.35 | +0.02em | 时间戳、工具耗时、token 数、键帽、徽章内文字 |
| `label` | 12 | 500–600 | 1.45 | +0.01em | 表单标签、工具卡元信息、按钮小号、错误横幅正文 |
| `secondary` | 13 | 400–500 | 1.5 | 0 | 设置项标题、列表项、卡片副标题、次级按钮 |
| `body` | 14 | 400 | 1.55（中文 1.6） | 0 | **消息正文、输入框、主力阅读字号** |
| `title` | 16 | 600 | 1.35 | -0.01em（仅拉丁） | 区块标题、空状态标题、对话标题 |
| `heading` | 20 | 600 | 1.3 | -0.015em（仅拉丁） | 页面标题（设置/记忆/邮件） |
| `display` | 24 | 600 | 1.25 | -0.02em（仅拉丁） | 仅首次引导 / 空面板主标题，全产品 ≤2 处 |

规则：
- **正文最小 12 px，常用 14 px**；11 px 只允许出现在"短、非关键、可放大"的元信息里。
- 字重只有 400 / 500 / 600 / 700，禁用 300 与 800。
- 负字距只给 ≥20 px 的**拉丁**标题；中文标题 `letter-spacing: 0`（负字距会让汉字挤在一起）。
- 单行文本 `text-wrap: nowrap` + 省略号；正文 `text-wrap: pretty`，中文断行用 `line-break: strict`。
- 一段正文 ≥3 行时，行宽控制在 26–34 个汉字（424 px 面板 - 32 px 内边距 ≈ 392 px，14 px 下约 28 字，正好）。

---

## 4. 间距 / 圆角 / 阴影 / 动效

### 4.1 间距（4 px 基准，映射 Nova 的 16 px rem 阶梯）

| 值 | 名称 | 典型用途 |
| --- | --- | --- |
| 0 | none | 贴边、分隔线与容器齐平 |
| 2 | hairline | 图标与文字微调、focus ring 内缩、键帽内边距 |
| 4 | xs | 图标与标签间距、徽章内边距、行内元素 gap |
| 8 | sm | 按钮水平内边距的一半、列表项内 gap、卡片内小间距 |
| 12 | md | 卡片内边距（紧凑）、列表行水平内边距、消息块之间 |
| 16 | lg | **内容区左右留白**、卡片内边距（标准）、区块内间距 |
| 24 | xl | 区块之间（消息与消息、设置分组） |
| 32 | 2xl | 大区块之间、页面顶部留白 |
| 40 | 3xl | 空状态上方留白 |
| 48 | 4xl | 标题栏高度、空状态顶部留白 |

Nova 对应：`xxsmall 2 / xsmall 4 / small 8 / medium 12 / large 16 / xlarge 24 / xxlarge 32`。我们额外加了 40/48，
只用于"面板几何"（空状态留白、标题栏），不进入控件内部。

### 4.2 圆角

| 值 | Token | 用途 | Nova 对应 |
| --- | --- | --- | --- |
| 4 | `xs` | 键帽、状态点容器、徽章、微标签 | `border.radius.xsmall` 4 px |
| 8 | `sm` | **按钮、输入框、导航项、图标按钮背景** | `small` 8 px |
| 12 | `md` | **卡片、工具调用卡、消息气泡、错误横幅、输入区容器** | `medium` 12 px |
| 16 | `lg` | **面板外框、浮层/菜单、设置分组大卡** | `large` 16 px |
| 24 | `xl` | 仅整页 sheet（设置/引导）顶部圆角 | `xlarge` 24 px（Nova 覆写值） |
| 9999 | `pill` | 状态芯片、开关、滚动条、头像（圆形） | `radius.circle` |

硬约束：**容器 ≤16，卡片 ≤12，控件 ≤8**；禁止 20 / 28 / 32 这类中间值（不在档位上就会显得"随手画的"）。
圆角半径永远不超过高度的一半（40 px 高的卡片不要 24 圆角）。

### 4.3 阴影 / 高度层级

| 层级 | 值（light） | 用途 |
| --- | --- | --- |
| L1 | `0 1px 2px rgba(0,0,0,.15), 0 0 1px rgba(0,0,0,.15)` | 键帽、悬浮提示、下拉的第一层 |
| L2 | `0 2px 6px rgba(0,0,0,.06), 0 .5px 1px rgba(0,0,0,.05)` | 菜单、Popover、工具卡的 hover 抬升 |
| L3 | `0 3px 12px rgba(0,0,0,.10), 0 1px 2px rgba(0,0,0,.06)` | 模态、抽屉、设置 sheet |
| Panel | `0 4px 16px rgba(0,0,0,.14), 0 12px 40px rgba(0,0,0,.18), 0 0 0 1px rgba(0,0,0,.20)` | **浮窗本体**（暗色各层 alpha +50%） |

规则：
- **静态卡片不加阴影**，用 1 px `border_subtle` 分层（Nova 的做法：靠表面色差与发丝边，不靠投影）。
- 阴影只在"元素脱离平面"时出现：菜单、拖拽中的卡片、Popover、悬浮的输入建议。
- 不允许彩色阴影；唯一例外是焦点环 `0 0 0 3px` 的 accent 24% 透明版本。
- 面板本体额外需要 1 px 外描边：暗色 `#ffffff14`，亮色 `#00000014`，否则在浅色桌面上会"糊"进背景。

### 4.4 动效

| Token | 值 | 用途 |
| --- | --- | --- |
| `instant` | 0 ms | 按下态、键盘操作反馈 |
| `fast` | 90 ms | hover 变色、图标按钮、状态点 |
| `base` | 140 ms | 开关、复选、标签切换、折叠标题 |
| `slow` | 200 ms | 工具卡展开/收起、气泡出现 |
| `sheet` | 280 ms | 设置/记忆/邮件整页切换（位移 8 px + 淡入） |
| `stream` | 1100 ms | 流式光标闪烁（step-end，非缓动） |

缓动（Nova 原始两条，别自创）：
- `standard: cubic-bezier(0.2, 0.8, 0.2, 1)` —— 颜色、尺寸、位移。
- `emphasized: cubic-bezier(0.2, 0, 0, 1)` —— 整页切换、面板出现、sheet。
- 禁止弹性/回弹（overshoot）、禁止 `ease-in-out` 默认值、禁止 >320 ms 的交互转场。
- `prefers-reduced-motion: reduce` 时全部时长归 0，仅保留流式光标（改为静态方块）。

---

## 5. 组件规格

> 约定：颜色 token 均指第 2 节；`r8` 表示圆角 8 px；"高"指控件外框高度；所有控件最小点击区 28×28，推荐 32×32。

### 5.1 按钮

| 变体 | 高 | 内边距 | 圆角 | 背景 | 文字 | 边框 | hover | 按下 | 禁用 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 主按钮 | 32 | 0 12 | 8 | `accent` | `on_accent` / 13 px 600 | 无 | `accent_hover` | `accent_pressed` | `selection` 底 + `text_tertiary` 字 |
| 次按钮 | 32 | 0 12 | 8 | `surface_raised` | `text_primary` / 13 px 600 | 1 px `border_strong` | `selection` | `selection` + 边框 `accent` | 同上，边框 `border_subtle` |
| 幽灵按钮 | 28–32 | 0 8 | 8 | 透明 | `text_secondary` / 13 px 500 | 无 | `selection` 底，文字 `text_primary` | `selection` | `text_tertiary` |
| 图标按钮 | 28 或 32 | 0 | 8（方形）/ pill（圆形） | 透明 | `text_secondary`，图标 16 | 无 | `selection` | `selection` + 图标 `text_primary` | `text_tertiary` |
| 危险按钮 | 32 | 0 12 | 8 | `danger_bg` | `danger` / 13 px 600 | 1 px `danger` 30% | `danger` 12% 叠加 | `danger` 20% 叠加 | — |

- 焦点：`outline: 2px solid focus_ring; outline-offset: 1px; border-radius: 8px`（图标按钮同）。
- 主按钮**每屏最多 1 个**（发送按钮除外）；"重试/取消"一律次按钮或幽灵按钮。
- 文字 + 图标时：图标 16 px、gap 6 px、图标在左（关闭类在右）。

### 5.2 输入框

| 项 | 单行输入 / 搜索 | 多行输入（输入区） |
| --- | --- | --- |
| 高 | 32（搜索 32，带图标时左内边距 32） | 44 起，随内容增长，上限 128（约 5 行） |
| 内边距 | 0 12 | 8 12 |
| 圆角 | 8 | 12 |
| 背景 | `surface` | `surface` |
| 边框 | 1 px `border_subtle` | 1 px `border_subtle` |
| 文字 / 占位 | 14 `text_primary` / `text_tertiary` | 14 `text_primary` / `text_tertiary` |
| hover | 边框 `border_strong` | 同左 |
| focus | 边框 `focus_ring` + `0 0 0 3px` accent 24% | 同左（环在容器上，不在光标行） |
| 错误 | 边框 `danger` + 环 `danger` 20% + 下方 12 px `danger` 文案 | 同左 |
| 禁用 | 背景 `bg`，文字 `text_tertiary`，边框 `border_subtle` | — |

- 输入区容器（含工具条）整体 12 px 圆角、`surface` 底、1 px `border_subtle`；聚焦时整个容器亮起焦点环。
- 无 emoji 快捷插入、无 sparkles 装饰按钮；发送键为 28×28 图标按钮（`arrow-up`），空内容时禁用（`selection` 底 + `text_tertiary` 图标）。

### 5.3 卡片

| 项 | 标准卡 | 紧凑卡（设置分组内） |
| --- | --- | --- |
| 背景 | `surface_raised` | `surface_raised` |
| 边框 | 1 px `border_subtle` | 无（由分组容器提供） |
| 圆角 | 12 | 12 |
| 内边距 | 16 | 12 |
| 阴影 | 无（静态） | 无 |
| 卡内间距 | 标题→正文 8，正文→操作 12 | 6 / 8 |
| 标题 | 13 px 600 `text_primary` | 13 px 500 `text_primary` |
| 正文 | 13 px 400 `text_secondary`，行高 1.5 | 12 px 400 `text_secondary` |

嵌套上限 2 层（面板 > 卡片 > 行），第 3 层改用分隔线而不是新卡片。

### 5.4 工具调用卡片（三态，零 emoji）

结构：`左侧 2 px 状态轨 + 头部（状态点 + 图标 + 名称 + 元信息）+ 参数预览（等宽，≤2 行）+ 操作区`。

| 属性 | 值 |
| --- | --- |
| 容器 | `surface_raised`，r12，1 px `border_subtle`，内边距 8 12 |
| 状态轨 | 左侧 2 px 全高，`accent`（运行）/ `success`（成功）/ `danger`（失败） |
| 状态点 | 6×6 圆形，同状态色；运行态 opacity 1↔0.4 呼吸 1100 ms |
| 图标 | 16 px Lucide：`loader-2`（旋转 900 ms linear）/ `circle-check` / `circle-alert` |
| 名称 | 13 px 600 `text_primary`，单行省略 |
| 元信息 | 11 px `caption` `text_tertiary`，等宽，`tabular-nums`（例：`1.2 s · 812 tok`） |
| 参数预览 | 12 px `font-mono` `text_secondary`，最多 2 行，超出折叠 |
| 展开 | 头部整行可点，尾部 `chevron-down` 16 px，展开 200 ms `standard` |
| 悬停 | 边框 `border_strong`，不位移、不加阴影 |

| 状态 | 头部文案 | 图标 | 颜色 | 操作区 |
| --- | --- | --- | --- | --- |
| 运行中 | `运行中 · 正在读取收件箱` | `loader-2`（旋转） | `accent` | 幽灵按钮 `取消`（hover 才出现） |
| 成功 | `完成 · 1.2 s` | `circle-check` | `success` | 幽灵按钮 `查看输出`，次按钮 `重新运行` |
| 失败 | `失败 · 请求超时` | `circle-alert` | `danger` | 次按钮 `重试`，幽灵按钮 `查看日志` |

- 失败态在参数预览下方追加 2 行错误摘要（12 px `font-mono`，`danger`），超出折叠为"查看日志"。
- 连续 ≥3 个工具调用折叠为一行汇总：`● 3 个工具调用 · 全部成功` + 展开（12 px，`text_secondary`）。
- **禁止**：彩色背景整卡填充、闪烁整卡、进度条抖动、任何 emoji（含勾/叉/警告符号）。

### 5.5 消息气泡 / 消息块 / 流式

| 元素 | 规格 |
| --- | --- |
| 助手头像 | 24×24，r8，`surface_raised` 底，1 px `brand_hairline` 描边（200% 缩放下 1.5 px），内部 `sparkles` 图标 12 px `accent` |
| 助手消息块 | 不画气泡：整宽 - 头像 24 - gap 8，14 px/1.6 `text_primary`；编号列表/代码块内嵌 |
| 用户消息 | 右对齐，最大宽 88%，气泡 `surface_raised`，r12，内边距 8 12，14 px `text_primary` |
| 消息间距 | 不同发言者 16；同一发言者连续消息 4 |
| 头部行 | 12 px 600 名称 + 11 px `text_tertiary` 时间戳；时间只在每组第一条显示 |
| 悬停操作 | 24×24 幽灵图标按钮：`copy` / `rotate-ccw` / `more-horizontal`，出现在消息底部右侧，gap 2 |
| 流式光标 | 2×14 矩形 `accent`，`stream` 1100 ms step-end；输出结束后消失，不留占位 |
| 思考中 | 14 px `loader-2` 旋转 + 12 px `text_secondary` "正在思考"；≤600 ms 不显示（避免闪烁） |
| 代码块 | 12 px `font-mono`，`bg` 底，r8，内边距 8 12，顶部语言标签 11 px `text_tertiary`，右上角复制按钮 |
| 表格 | 最多 3 列；超出改为纵向键值列表；表头 11 px 600 `text_tertiary`，行高 32 |

**零 emoji**：助手语气靠排版（加粗关键词、短段落、列表）表达，不靠表情符号；用户输入中的 emoji 原样显示，但
不因此改变行高（预留 1.2 行高）。

### 5.6 标签页 / 主导航

| 项 | 值 |
| --- | --- |
| 容器 | 高 40，水平内边距 12，底部 1 px `border_subtle`，背景 `bg` |
| 项 | 高 32，内边距 0 8，gap 6，r8，图标 16 + 文字 13 px 500 |
| 未选中 | 文字 `text_secondary`，图标同色；hover 背景 `selection`，文字 `text_primary` |
| 选中 | 背景 `surface_raised`，文字 `text_primary` 600，1 px `border_subtle`；底部 2 px `brand_hairline` 下划线（宽 = 内容宽，距底 1 px，r pill） |
| 计数徽章 | 18 px 高 pill，内边距 0 5，11 px 600，`selection` 底 + `text_secondary` 字 |
| 切换动效 | 背景/文字 140 ms `standard`；下划线位移 200 ms `standard` |

同一套视觉用于设置页内的分段控件（segmented）：容器 `surface` 底、r8、内边距 2；项高 28、r8。

> Nova 依据：选中标签去掉阴影，改用 `tab_line` + 选中面；Nova 选中标签的 leading/trailing 分别是 violet/orange 发丝。
> 我们把这条语言降维成导航项底部的 2 px 紫→橙下划线，只在"当前页"出现一次。

### 5.7 开关

| 项 | 值 |
| --- | --- |
| 尺寸 | 轨道 36×20，r pill；滑块 16×16 圆形，距边 2 |
| 关 | 轨道 `border_strong`，滑块 `surface_raised` |
| 开 | 轨道 `accent`，滑块 `on_accent` |
| hover | 轨道叠 8% 当前色；focus 环同按钮 |
| 动效 | 滑块位移 140 ms `emphasized`，颜色 140 ms `standard` |
| 行 | 高 44，标签 13 px `text_primary` + 描述 12 px `text_secondary`，控件右对齐 |

### 5.8 滚动条

| 项 | 值 |
| --- | --- |
| 轨道 | 10 px，透明（`scrollbar_track` = `#00000000`） |
| 滑杆 | 6 px，r pill，距边 2 px，`scrollbar_thumb`；hover 时 `text_tertiary` |
| 最小长度 | 32 px |
| 行为 | 悬浮式（overlay），静止 800 ms 后淡出至 0（120 ms），滚动时立即出现；不占据内容宽度 |
| 内容区 | `scrollbar-gutter: stable` 关闭（浮窗宽度宝贵，用 overlay） |

### 5.9 分隔线

| 类型 | 值 |
| --- | --- |
| 水平 | 1 px `border_subtle`，左右内缩 12（与文字对齐）；全宽仅用于导航栏底部 |
| 垂直 | 1 px，高 16，颜色同上，用于工具条内分组 |
| 带标题分组 | 标题 11 px 600 `text_tertiary` 大写仅英文，行高 24，上下间距 8 |

禁止 2 px 以上的装饰性分隔条；状态轨（2 px）是唯一的例外，因为它编码状态。

### 5.10 空状态

| 项 | 值 |
| --- | --- |
| 布局 | 垂直居中，最大宽 280，整体上移 8%（视觉重心略高） |
| 图标盘 | 44×44 圆形，`surface_raised` 底，1 px `border_subtle`，内部 20 px 线性图标 `text_tertiary` |
| 标题 | 14 px 600 `text_primary`，与图标间距 16 |
| 说明 | 13 px `text_secondary`，行高 1.6，居中，最多 2 行 |
| 操作 | 1 个次按钮（32）或 2 个建议 chip（28，r8，`surface_raised` + 1 px `border_subtle`） |
| 文案 | 陈述能力，不卖萌：`还没有对话` / `可以让我读邮件、查记忆或调用工具` |

### 5.11 错误态

| 层级 | 规格 |
| --- | --- |
| 行内错误（输入框） | 边框 `danger`，下方 12 px `danger` 文案，间距 4 |
| 横幅 | r12，`danger_bg` 底，1 px `danger` 30% 边，内边距 8 12；16 px `circle-alert` + 13 px 600 标题 + 12 px 正文 + 右侧文字按钮 `重试` |
| 卡片级失败 | 工具卡失败态（见 5.4），保留已产出的部分结果，不整块替换为错误 |
| 整页失败 | 图标盘（`circle-alert`，`danger`）+ 16 px 600 `无法连接到本地 Agent` + 12 px `font-mono` 错误码（最多 2 行，省略）+ 主按钮 `重试` + 幽灵按钮 `查看日志` |
| 断流 | 消息块尾部追加一行：`● 连接中断 · 已保留部分回复`（12 px，`warning` 点 + `text_secondary` 字）+ `继续` 文字按钮 |

错误文案规范：**说清"发生了什么 + 能做什么"**，不写堆栈；堆栈只进"查看日志"。

---

## 6. 整体布局栅格

### 6.1 面板几何

| 项 | 值 |
| --- | --- |
| 宽 | 424 px（允许 400–440；小于 400 时正文掉到 12 px，不建议） |
| 高 | 屏幕工作区 88%（clamp 640–980） |
| 位置 | 距屏幕右缘 12，垂直居中；多屏跟随鼠标所在屏 |
| 外框 | r16，1 px 描边（暗 `#ffffff14` / 亮 `#00000014`），Panel 阴影 |
| 内部网格 | 单列：标题栏 48 / 主导航 40 / 内容 flex / 输入区 auto（92–176） |
| 内容左右留白 | 16（列表行可到 12，卡片到 16） |
| 滚动 | 仅内容区滚动；标题栏、导航、输入区固定 |

### 6.2 纵向节奏

```
48  标题栏（拖动区，含窗口按钮）
40  主导航（对话 / 记忆 / 邮件 + 设置入口）
─   1 px border_subtle
flex 内容区（消息流 / 列表 / 表单），padding 16 上下 12
auto 输入区（textarea 44→128 + 工具条 28 + gap 4 + 上下内边距 16）
```

内容区顶部留白 12，消息之间 16；输入区上方 12 的分隔由 1 px 线承担（不加阴影）。

### 6.3 线框图（ASCII）

主视图（对话）：

```
+----------------------------------------------+    424 px
| [N] NeoSonia                 [pin] [min] [x] |  48  标题栏 / 拖动区
+----------------------------------------------+
| [ 对话 ] [ 记忆 ] [ 邮件 ]            [gear] |  40  主导航（激活项 2 px 紫->橙）
+----------------------------------------------+
|                                              |
|  (A)  助手                            12:04  |  头像 24 / 名称 12 / 时间 11
|       我先读一下你的收件箱。                 |  正文 14 / 行高 1.6
|       +----------------------------------+   |
|       | * 读取收件箱              1.2 s  |   |  工具卡：状态轨 2 px + 点 + 图标
|       |   folder: inbox                  |   |
|       +----------------------------------+   |
|                                              |
|                       +--------------------+ |
|                       | 总结一下未读邮件   | |
|                       +--------------------+ |  用户气泡：右对齐，最大 88%
|                                              |
|  (A)  助手                            12:05  |
|       已归为 3 组：账单、日程、待办。        |
|       [ 复制 ] [ 重新生成 ]                  |  hover 才出现
|                                              |
+----------------------------------------------+
| +------------------------------------------+ |
| |  输入指令，Enter 发送            [send]  | |  输入区 r12，整框聚焦亮环
| +------------------------------------------+ |
| Enter 发送 · Shift+Enter 换行    Nova · 本地 |  11 px 提示行
+----------------------------------------------+
```

说明：`(A)` 是头像占位，`*` 是状态点，方括号内是图标/控件占位；本图不含任何 emoji。

设置页（整页 sheet，280 ms 切页）：

```
+----------------------------------------------+
| [<] 设置                                 [x] |  48  返回 + 标题 + 关闭
+----------------------------------------------+
| 外观                                         |  分组标题 11/600 tertiary
| +------------------------------------------+ |
| |  主题           [跟随系统] [深色] [浅色] | |  分段控件 28
| |  强调色                  [紫] [跟随系统] | |  chip
| |  密度                      [舒适] [紧凑] | |  分段 / 开关
| +------------------------------------------+ |  卡片 r12 · padding 12
| 模型与记忆                                   |
| +------------------------------------------+ |
| |  默认模型                   GPT-5   [v]  | |  行高 44
| |  记忆                            [ on ]  | |  开关 36x20
| +------------------------------------------+ |
| 快捷键                                       |
| +------------------------------------------+ |
| |  唤起浮窗              Ctrl+Shift+Space  | |
| |  切换页面                Ctrl+1 / 2 / 3  | |  键帽 18 · r4
| +------------------------------------------+ |
+----------------------------------------------+
```

### 6.4 切页与导航模型

| 动作 | 方式 | 反馈 |
| --- | --- | --- |
| 对话 / 记忆 / 邮件 | 顶部主导航（三选一），`Ctrl+1/2/3` | 内容交叉淡入 140 ms；导航下划线滑动 200 ms；输入区位置不变 |
| 设置 | 标题栏 `gear` 图标 / `Ctrl+,` | 整页 sheet 从右侧推入 8 px + 淡入，280 ms `emphasized`；`Esc`/返回箭头回到原页 |
| 设置内的子页 | 行内下钻（列表 → 详情），标题栏变 `[<] 标题` | 同 sheet 动效；保留滚动位置 |
| 关闭 / 隐藏 | `Esc`（输入框有内容时先清空再 Esc）、标题栏 `x` | 200 ms 淡出 + 4 px 下移；`Ctrl+Shift+Space` 重新唤起 |
| 置顶 | 标题栏 `pin` 切换 | 图标由 `pin` → `pin-off`，状态用文字 tooltip 说明，不弹 toast |

状态保留：每个页面各自保留滚动位置与草稿；切页不重建输入框（草稿不丢）。

---

## 7. `docs/ui-design-palette.json` 与 Rust 映射

JSON 结构（snake_case，可直接 `include_str!`）：

```json
{
  "dark":  { "bg": "#121114", "surface": "#171519", "...": "..." },
  "light": { "bg": "#f7f6fb", "surface": "#fcfbff", "...": "..." },
  "spacing": [0, 2, 4, 8, 12, 16, 24, 32, 40, 48],
  "radius":  { "none": 0, "xs": 4, "sm": 8, "md": 12, "lg": 16, "xl": 24, "pill": 9999 },
  "motion_ms": { "instant": 0, "fast": 90, "base": 140, "slow": 200, "sheet": 280, "stream": 1100 },
  "easing":  { "standard": "cubic-bezier(0.2, 0.8, 0.2, 1)", "emphasized": "cubic-bezier(0.2, 0, 0, 1)" },
  "elevation": { "level_1": "...", "level_2": "...", "level_3": "...", "level_4_panel": "..." },
  "typography_px": { "caption": 11, "label": 12, "secondary": 13, "body": 14, "title": 16, "heading": 20, "display": 24 }
}
```

最小可用解析（`crates/neosonia-app/src/theme.rs`）：

```rust
use serde::Deserialize;

const RAW: &str = include_str!("../../../docs/ui-design-palette.json");

#[derive(Debug, Clone, Copy)]
pub struct Rgba { pub r: u8, pub g: u8, pub b: u8, pub a: u8 }

impl TryFrom<String> for Rgba {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let h = s.strip_prefix('#').ok_or_else(|| format!("bad hex: {s}"))?;
        let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map_err(|e| e.to_string());
        match h.len() {
            6 => Ok(Rgba { r: p(0)?, g: p(2)?, b: p(4)?, a: 255 }),
            8 => Ok(Rgba { r: p(0)?, g: p(2)?, b: p(4)?, a: p(6)? }),
            _ => Err(format!("bad hex len: {s}")),
        }
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Rgba::try_from(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Radius { pub none: i32, pub xs: i32, pub sm: i32, pub md: i32,
                    pub lg: i32, pub xl: i32, pub pill: i32 }

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Motion { pub instant: u16, pub fast: u16, pub base: u16,
                    pub slow: u16, pub sheet: u16, pub stream: u16 }

#[derive(Debug, Clone, Deserialize)]
pub struct Easing { pub standard: String, pub emphasized: String }

#[derive(Debug, Clone, Deserialize)]
pub struct Elevation { pub level_1: String, pub level_2: String,
                       pub level_3: String, pub level_4_panel: String }

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TypeScale { pub caption: u8, pub label: u8, pub secondary: u8,
                       pub body: u8, pub title: u8, pub heading: u8, pub display: u8 }

pub fn load() -> Palette { serde_json::from_str(RAW).expect("ui-design-palette.json") }

#[derive(Debug, Clone, Deserialize)]
pub struct Palette {
    pub dark: ThemeColors,
    pub light: ThemeColors,
    pub spacing: [u16; 10],
    pub radius: Radius,
    pub motion_ms: Motion,
    pub easing: Easing,
    pub elevation: Elevation,
    pub typography_px: TypeScale,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ThemeColors {
    pub bg: Rgba, pub surface: Rgba, pub surface_raised: Rgba, pub overlay: Rgba,
    pub border_subtle: Rgba, pub border_strong: Rgba,
    pub text_primary: Rgba, pub text_secondary: Rgba, pub text_tertiary: Rgba,
    pub accent: Rgba, pub accent_hover: Rgba, pub accent_pressed: Rgba, pub on_accent: Rgba,
    pub brand_from: Rgba, pub brand_to: Rgba,
    pub success: Rgba, pub success_bg: Rgba, pub warning: Rgba, pub warning_bg: Rgba,
    pub danger: Rgba, pub danger_bg: Rgba, pub selection: Rgba, pub focus_ring: Rgba,
    pub scrollbar_thumb: Rgba, pub scrollbar_track: Rgba, pub scrim: Rgba,
}
```

落地要点：

1. **hex 是 sRGB 真值，OKLCH 用于 ramp**。启动时把 hex 转一次 OKLCH（sRGB→linear→OKLab→LCh，Björn Ottosson 公式），
   所有派生色（hover 叠色、焦点环 24%、状态轨 12%）在 OKLCH 里做 L/C 调整后再转回 sRGB 并做色域裁剪，
   避免在 sRGB 里线性插值产生"发灰"的中间色。文档中每色的 `oklch(L% C H)` 即为该转换的目标值。
2. `radius.pill = 9999` 在绘制前 clamp 到 `min(w, h) / 2`，不要直接当像素用。
3. `spacing` 与 fenestra 的 `SP*` 常量对齐：`spacing[5] == 16 == SP4`，`spacing[6] == 24 == SP6`；
   保留 JSON 为唯一真值，不要在代码里再写死数字。
4. **窗口圆角**：若走 DWM（`DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND`），系统会给约 8–11 px 圆角 + 系统阴影，
   与设计值 16 有差；要精确 16 px 需自绘圆角（分层窗口 + 区域裁剪），并自行绘制 Panel 阴影与 1 px 描边。
   两种方案都接受，但**同一版本内必须统一**，不要一半系统圆角一半自绘。
5. 主题切换：订阅系统 `prefers-color-scheme`；"跟随系统 accent" 是可选设置项，开启时用系统 accent 替换 `accent`
   并同步推导 hover/pressed（暗色下压暗 10%、亮色下压暗 10%，与 Acorn 的 `nativeTheme` 规则一致）。

---

## 8. 不要做的事

1. **不要 emoji**：状态、标题、空状态、错误、按钮、提示、日志，一律零 emoji。要表达情绪用排版（加粗、短句、节奏）。
2. **不要彩色渐变滥用**：全产品只允许一条 `#b89cff → #ff9565` 的 2 px 发丝线（导航下划线 / 头像描边）。
   禁止背景渐变、卡片渐变、文字渐变、蓝紫"AI 感"渐变。
3. **不要毛玻璃过度**：默认纯色。最多允许标题栏 8 px blur；卡片、输入区、菜单一律不 blur。
   大面积 blur 会让 400 px 宽的面板变成"糊成一片"，也拖累渲染性能。
4. **不要圆角超限**：容器 ≤16、卡片 ≤12、控件 ≤8；禁止 20/28/32 等非档位值；不要把所有东西做成 pill。
5. **不要纯黑纯白**：暗色底不用 `#000000`（用 `#121114`），亮色卡片不用"死白 + 灰边"组合（用 `#fcfbff` / `#ffffff` + 紫灰边）。
6. **不要用颜色单独编码状态**：红/绿点必须配图标与文字；去掉颜色后仍要能读懂。
7. **不要三层嵌套卡片**，也不要给每张卡都加阴影；静态层级用 1 px 发丝边 + 表面色差。
8. **不要在 424 px 里做双栏、多列工具条或左侧 56 px 图标栏**：宽度只够单列 + 顶部导航。
9. **不要小于 12 px 的正文**，不要 300/800 字重，不要全大写英文长句，不要字距 >0.04em。
10. **不要彩色阴影/发光**（焦点环除外），不要投影里带品牌色。
11. **不要弹性动画**：无 overshoot、无弹簧、无 >320 ms 的交互转场；必须响应 reduced-motion。
12. **不要复刻 WinUI 控件外观**（不要在应用内再造一遍 Windows 设置页）；我们借的是系统习惯，不是系统皮。
13. **不要在消息流里塞大表格/大卡片**：>3 列改键值列表，>6 行折叠；工具卡折叠后 ≤2 行预览。
14. **不要 emoji 充当图标占位**，也不要用"图标 + 同义文字"重复同一信息（例如图标已是勾选，文字不要再写"成功成功"）。
15. **不要隐藏键盘路径**：所有主要动作必须有快捷键或可 Tab 到达；focus 环不得因 `outline: none` 消失。

---

## 9. 交付自检清单

- [ ] 暗/亮两套色板均取自本文件 token，无临时色值。
- [ ] 每个状态（运行/成功/失败/断流/空）都有"点 + 图标 + 文字"。
- [ ] 任意页面截图去色后，层级仍可辨（标题栏 / 导航 / 内容 / 输入区）。
- [ ] 424 px 宽度下，中文正文 26–34 字/行，无横向滚动。
- [ ] 所有圆角在 {4, 8, 12, 16, 24, pill} 内；所有间距在 {2,4,8,12,16,24,32,40,48} 内。
- [ ] 焦点环在键盘 Tab 下 100% 可见，且对比 ≥3:1。
- [ ] 无 emoji、无大面积渐变、无卡片 blur。
- [ ] `docs/ui-design-palette.json` 可被 `include_str!` + serde 直接解析，字段名与本文一致。
