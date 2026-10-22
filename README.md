# 消息面分析系统

收集加密市场消息，分级、总结并支持多消息对照追问。Rust 本地服务 + Tauri 桌面应用，界面资源直接编译进程序。

## 当前版本

桌面版本 **0.2.1**，正式界面对齐 [最新原型 v17](docs/prototypes/feed-first-v17.html)。顶部三类概览（紧急事件、重要利空、重要利好），下方连续消息流；消息详情与追问合为一个会话，支持多条参考消息、移出及撤销、流式回答、自动刷新倒计时。

重点速览显示原始发布后 6 小时内的紧急事件、24 小时内的重要利空/利好；消息流可继续向下读取更早历史。页面每 30 秒同步已采集数据，信源按各自采集周期运行。

## 文档

- [设计](docs/design.md)
- [数据与接口](docs/data-and-interfaces.md)
- [安装与运行](docs/install.md)
- [信源调研](docs/research.md)
- [代码分层与交互架构图](docs/architecture/architecture.html)
- [0.2.1 验证记录与验收边界](docs/verification/release-0.2.1.md)

## 开发验证

```powershell
cargo run -p app --release -- serve
cargo test -p web -p agent -p collect --release
cargo clippy --workspace --all-targets --release -- -D warnings
```

## Windows 打包

在项目根目录运行 `./package.ps1`，编译最新代码后将安装包、SHA-256、更新签名和 `latest.json` 输出到 `release/`。发行文件托管在 GitHub Releases，旧安装包在新包导出校验后清理。

`./package.ps1 -StageExisting` 仅导出已有构建，不会更新界面代码；日常发布请使用默认完整构建。

发布脚本设置输出目录和安装包为正常用户完整性级别，避免开发目录的低完整性标记导致 NSIS 无法写入临时目录。

历史任务卡、旧阶段规格、旧原型及重复备份已移出项目。保留当前源代码、必要开发示例和当前维护文档；用户运行数据保存在系统用户数据目录。

## 分栏与微信会话

消息流与追问区之间可拖拽调整比例，双击分隔条恢复默认；追问区可收起，布局会保留。点击消息会自动展开追问区。

升级后收到的微信问答会自动出现在会话列表，可在应用中继续同一段会话；之后微信追问也会读取这段历史。应用中的续问保存在共享记录中，不会自动把桌面回答再次发到微信。旧版未保存的微信问答无法回补。

## 许可证

本项目采用 [Apache License 2.0](LICENSE)，允许商业使用、修改和再分发；使用时须遵守许可证及第三方组件的许可要求。
