# 消息面分析系统

设计文档以 `docs/design.md`（主设计）、`docs/data-and-interfaces.md`（数据与接口）为准，`docs/research.md` 是调研记录。实现与设计冲突时，先停下来报告，不要自行改设计。

## 当前入口

- 最新界面原型：`docs/prototypes/feed-first-v17.html`。
- 正式界面：`crates/web/templates/main.html` 和 `crates/web/assets/main-v17.css`，编译嵌入应用。
- 安装包统一通过根目录 `package.ps1` 输出到 `release/`。
- 历史任务卡已清理；以当前用户确认的任务范围为准。

## 技术栈

只用 Rust 一种语言。可执行文件 `app` 的子命令是 `serve` 和 `plugin <名字>`。插件框架叫 `hub`，自写、最小化复刻 Cordis，一切功能都是插件：官方插件编译进 `app`，作为一个程序发布；扩展插件是插件目录下的单独可执行文件，带 `plugin.toml` 清单，经 JSON-RPC 与 hub 通信，重启后加载，不做热重载。两种插件用同一个 `plugin-sdk` 写。不引入 Node 和前端构建工具。新增依赖必须先经负责人同意。

- 桌面外壳：Tauri 2，放在独立的 `desktop` crate。它只负责窗口、托盘、开机自启、系统通知、单实例、自动更新和安装包，窗口加载本机界面。`app`、hub 和所有插件都不能依赖 Tauri；不用 Tauri 时 `app serve` 必须能单独运行。

- 异步与网络：tokio、axum、reqwest
- 数据库：SQLite + sqlx，只用运行时查询，不用编译期 SQL 宏
- 界面：minijinja 模板 + htmx，对话流式输出用 SSE；静态资源编译时嵌入程序
- 序列化与配置：serde、serde_json、toml
- 错误与日志：thiserror、tracing
- 加密：aes-gcm、keyring
- 信源解析：feed-rs、scraper；浏览器自动化：chromiumoxide；微信：自写 iLink 协议客户端（`crates/ilink`）
- 模型：只有 OpenAI 兼容（默认，接国内服务）和 Anthropic 原生（可选）两个适配器
- 网络：核心和所有插件的外部请求统一走配置里的代理，不允许某个请求绕过。官方插件共用一个必须带代理的 HTTP 客户端；扩展插件进程由 hub 注入代理环境变量
- 测试：cargo test、wiremock；测试不访问真实网络

## 当前范围：MVP

当前只做 MVP，范围和阶段以设计文档"MVP 阶段"为准。设计文档"后续扩展"清单里的功能属于后续迭代，当前任务未要求的不做；但实现时不要堵死这些功能的扩展路径。

## 首版范围：只做本机

首版只支持在用户电脑上运行。不实现任何服务器部署相关的功能：Docker 镜像、远程采集节点（`app node`）、WebSocket 远程传输、HTTPS 与配对码、环境变量或密钥文件提供主密钥、数据迁移。

但必须保留这些扩展点，不能写死：

- 核心是独立的后台服务，界面只通过 HTTP 接口访问核心
- hub 的进程插件传输层是 trait，首版只有 stdio 实现
- 主密钥来源是 trait，首版只有系统钥匙串实现
- 监听地址从配置读取，首版默认且只允许本机地址
- 认证和权限各有统一入口，首版恒为主人
- 个人数据带用户 ID；数据库访问走 store 抽象

## 体积约束

目标是尽量减小可执行文件体积：

- 所有依赖写 `default-features = false`，只开实际用到的 feature。
- TLS 统一用 rustls（ring 后端），不用 native-tls / OpenSSL。
- 同类库只用一个：时间只用 `time`，错误只用 `thiserror`，不引入 anyhow、chrono。
- tracing-subscriber 不开 `env-filter`（避免引入 regex），日志级别用配置项控制。
- 不打包、不下载任何浏览器：chromiumoxide 不开 `fetcher` 等会自动下载 Chromium 的 feature。X 插件按顺序查找用户电脑上已安装的 Chrome、Edge、Chromium，都找不到时停用 X 信源并提示用户安装，不自行下载。
- 界面不引入 CSS 框架，手写少量 CSS；只嵌入 htmx 一个前端脚本。
- 不使用 UPX 压缩（Windows 杀毒误报、macOS 签名问题）。
- 新增依赖前用 `cargo bloat --release --crates` 对比体积变化，写进任务结果。

## 架构约束

- 分层：应用层（`app`、`desktop`）→ 插件层 → 框架层（`hub`、`contracts`、`plugin-sdk`）。依赖方向只能是：应用层 → 插件层 → 框架层。插件之间禁止互相引用代码，只通过事件和服务接口交互；官方服务接口放在 `contracts`，扩展插件的接口放在它单独发布的接口 crate 里。
- 事件用于通知，服务用于调用。
- `contracts` 里的表结构、事件和服务接口只增不删；改动 `contracts` 必须由负责人完成。
- 每个插件只读写自己的表，所有数据访问都经 store 服务，任何插件都不能直接打开数据库。
- 所有时间以 UTC 存储。
- 密钥不得出现在日志、错误信息、测试快照里。

## 完成标准

每个任务结束前必须全部通过：

1. `cargo fmt --all`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. 涉及 crate 的 `cargo test --release`
4. 核对用户确认的任务范围，验证实际界面交互及发布产物

## 提交

- 所有改动直接提交到 master，不建任务分支，不用 worktree；并行任务的文件范围不能重叠，提交时只 add 自己的文件
- 提交信息用中文，第一行写清做了什么
- 不要推送到远端，由负责人合并
