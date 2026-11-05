# 第三方组件与许可

NeoSonia 本体以 MIT 许可发布（见 LICENSE）。

第三方依赖各自遵循其许可证（均为宽松开源许可，主要为 MIT / Apache-2.0）：

- fenestra / fenestra-core / fenestra-shell / fenestra-markdown / fenestra-kit（MIT 或 Apache-2.0）
- winit、wgpu、vello、parley、taffy（MIT / Apache-2.0）
- tokio、axum、tower-http、reqwest、serde（MIT / Apache-2.0）
- imap、lettre、mail-parser（Apache-2.0 / MIT）
- arboard、rfd、image、xcap、windows（MIT / Apache-2.0）

`vendor/` 目录内为 fenestra-core / fenestra-shell 的**本地 patch 版本**（IME 组合输入、
Win32 文件拖放、窗口行为修复），其上游 LICENSE / 版权声明随源码保留；
本项目对上游的改动说明见 `vendor/fenestra-shell/CHANGES-NEOSONIA.md` 与
`docs/fenestra-api.md`。

字体：界面使用系统中文字体（如微软雅黑），不随本仓库/安装包分发字体文件。