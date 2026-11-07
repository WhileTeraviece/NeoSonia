# NeoSonia v1.0.1 Release Notes

2026-11-07。

## 本版重点

### 计算机使用
- **Tab 键遍历**：`tab_scan` 列出屏幕内所有可 Tab 聚焦对象（属性 + 屏幕坐标），`tab_act` 按 index 执行 focus/click/enter/space/tab/shift_tab；自动过滤屏幕外/隐藏控件。
- **截图坐标自适应**：`screenshot` 输出坐标契约（图像尺寸/屏幕尺寸/scale/锚点），grid 图右下角标注「网格=屏幕物理px；图片px × N = 屏幕px」；`max_edge=0` 可返回原始分辨率。
- **鼠标工具吃图片坐标**：`mouse_move/click/drag/...` 新增 `coord_space:"image"`，按最近一次截图自动换算，避免手工乘缩放比例。

### 交互与界面
- 粘贴（Ctrl+V 图片/文件）只在前台可见且聚焦时生效，其他窗口粘贴不再被 NeoSonia 接收。
- 点任务栏关闭/标题栏 X 不再退出：隐藏到后台，Alt+Space/托盘唤回；托盘「退出」才结束进程。
- 历史页会话超过 3 条折叠：默认 3 条 + 「展开全部（N 条）」/「收起」。
- 网页远程端改为多 token 存储：一个浏览器保存多个配对令牌（去重、上限 8、旧键迁移）；某个 token 失效只移除它，全部失效也保留列表。

### 提示词
- 天气/日程/生活常识/闲聊/新闻等**日常内容必须先 `switch_mode {"mode":"daily"}` 再回答**，禁止在 coding/computer_use 模式直接回答。

## 兼容性
与 v1.0.0 配置/数据兼容；安装包不含用户数据/密钥。
