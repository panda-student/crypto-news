# 0.2.1 验证记录

验证日期：2026-10-22。公开源码：[panda-student/crypto-news](https://github.com/panda-student/crypto-news)。发行页：[v0.2.1](https://github.com/panda-student/crypto-news/releases/tag/v0.2.1)。

## 已完成

| 项目 | 结果与边界 |
| --- | --- |
| 编译与静态检查 | workspace release 模式，fmt、Clippy `-D warnings` 通过 |
| Rust 测试 | 454 项通过，0 失败；覆盖现有插件及 MCP 的准备/确认、并发、取消、过期、防重放、权限、密钥、内部模型共用计划与业务错误 |
| 安装卡住 | 运行中的旧版安装到 D:\CryptoNews，使用 0.2.0 修复包覆盖安装：退出码 0，约 5.91 秒完成，旧进程不再运行。0.2.1 保留相同安装退出逻辑 |
| 安装后界面 | 原生窗口截图，以及浏览器连接已安装 0.2.0 的真实本机服务。验证关注币种草稿/撤销、MCP 确认生效后展示已有项、移除与保存；测试数据已恢复 |
| X 管理 | 已登录状态、单独管理区、采集开关、搜索过滤、列表内部滚动。未新增真实 X 账号或重新执行账号登录 |
| 信源数量 | 同一份状态快照：订阅/公告 15/18、X 9/9，合计正常 24/27。3 个 SEC 来源仍缺 contact_email，不冒充正常 |
| 搜索筛选 | 操作组合关键词与排除词、同时选择 BTC/ETH、打开推送设置；本轮未发送真实测试推送 |
| 外部 MCP | 从已安装 desktop.exe 启动真实 stdio 子进程；initialize、tools/list、tools/call 成功，共 40 个工具；通知无 stdout；确认前配置不变，界面确认后生效 |
| 内部 AI | 使用已配置的真实 DeepSeek 生成“关注 DOGE”待确认卡片；点击取消，未改配置。非全部 38 种变更工具逐一真实执行的验收 |
| 更新发现 | 已安装 0.2.0 成功读取公开 GitHub latest.json，并显示可更新至 0.2.1 |
| 发行完整性 | 无认证的公开下载与 release 本地安装包 SHA-256 一致；独立调用 minisign-verify 验证签名成功；翻转安装包字节后校验失败 |
| 源码对应 | 发布快照与工作区所有 crates 源文件逐字节一致；本地私有 Git 历史、数据和配置不上传 |
| 架构图 | archify showcase 9/9 检查通过、0 警告；4 种桌面尺寸明暗主题检查通过；已检查大屏截图，没有连线穿过卡片或文字遮挡 |

安装包：`release/CryptoNews_0.2.1_x64-setup.exe`，9,210,923 字节。

SHA-256：`e38c8a7da757f94bad644438aadd52df5d67845ad38a1fe15dcec1b02a61eae6`。

架构图 HTML SHA-256：`ada014c9086df7ea71f58ac3cd25d4c441e8a760ef233d25730fdc950e123f50`。JSON 规格为 `docs/architecture/architecture-compact.json`，自动化浏览器证据为同目录 `architecture.visual-check.json`；该机器报告的 visualReview=pending 表示自动检查不代替视觉审查，本轮另行查看了大屏截图。

## 尚未验收

- 应用内“下载 → 安装 → 重启 → 数据保留”的完整升级流程：用户明确选择稍后再做。自动审批阻止代点安装更新，未绕过或用 API 代替点击。
- 对全部 MCP 工具的逐项真实 GUI 执行、外部 Claude/Cursor 等特定客户端配置验收；本轮已验证真实 stdio 与内部 DeepSeek 共用执行计划。
- Windows Authenticode 代码签名尚未配置；Tauri 更新签名已配置并验证。
- GitHub Windows CI 工作流已上传并触发，运行结果见[本次 CI](https://github.com/panda-student/crypto-news/actions/runs/39798153369)。本记录不把启动任务等同于 CI 全部通过。

## 本机证据

`target/acceptance-0.2.0/` 保存覆盖安装结果、MCP stdio JSON、原生窗口和各交互截图、公开下载文件及签名校验结果；`target/final-tests.log`、`target/final-clippy.log` 保存本地检查。证据目录被 Git 忽略，不作为用户运行数据迁移或新版本源码。

`cargo bloat` 未安装，因此未声称完成依赖体积对比；以实际发行包大小记录交付体积。
