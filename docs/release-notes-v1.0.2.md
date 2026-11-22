# NeoSonia v1.0.2 Release Notes

2026-11-22。

## 本版重点

### DeepSeek 文件解析 / OCR（新增）
- 使用 DeepSeek 官方 API Key 时，模型可调用三个新工具：
  - `deepseek_file_upload {path, purpose?, expires_seconds?}`：上传 PDF/Word/PPT/图片等并解析，返回 `file_id`（`text_recognition` / `rich_layout_recognition` / `user_data`）。
  - `deepseek_ocr {path, purpose?, max_pages?}`：调用 `POST /v1/tools/ocr`，按页返回识别文字（保留 `<｜ref｜>/<｜box｜>` 坐标标记），支持页数截断。
  - `deepseek_document_qa {file_id(s), question, model?}`：在对话中引用 `file_id` 提问，基于文档内容回答。
- 仅在 `base_url` 为 DeepSeek 官方端点（含默认空值）且解析到 API Key 时注册；支持 `env:NAME`/`${NAME}` 与 `NEOSONIA_API_KEY`/`DEEPSEEK_API_KEY` 环境变量回退。
- 工具目录 78 个。

### 修复
- 人设编辑页面长时间停留后的卡顿/卡死倾向（见验证记录）。

## 兼容性
与 v1.0.1 配置/数据兼容；安装包不含用户数据/密钥。
