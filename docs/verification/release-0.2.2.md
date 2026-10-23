# 0.2.2 发行验证

本版包含独立 operations 插件、Agent 经 Hub 调用 LLM/操作服务、共享预览确认机制、可选依赖启动修复与新版架构图。

- 源码回归：全工作区 475 项通过、0 失败、1 项既有忽略；最后补充 web/agent/operations 89 项回归通过。Clippy release 全工作区与 fmt 检查通过。
- 真实 Hub + app serve 浏览器验收：外部 stdio MCP、内部 Agent 本地模拟模型、确认前不修改、确认后生效、取消、防重放、插件开关、离线状态与重启持久化均通过。见 [操作插件验收](../architecture/operations-verification.md)。
- 本版重新执行 package.ps1 完整构建，不复用旧包。桌面 Cargo 与 Tauri 版本均为 0.2.2，输出到根目录 release。
- 安装包：CryptoNews_0.2.2_x64-setup.exe，9,335,159 字节；SHA-256：e21467ab50c004a1caed6269d1d8c660fc75005e94a9a253c34c625a694ced83。
- 保持现有 Tauri 更新公钥与 GitHub latest.json 地址；安装包签名通过原公钥校验，篡改后校验拒绝。
- 发行地址：[v0.2.2](https://github.com/panda-student/crypto-news/releases/tag/v0.2.2)。四个公开文件：安装包、.sig、.sha256、latest.json；发布后再下载核对字节和签名。

本轮发布不代表已经替用户执行安装。应用内“下载→安装→重启→数据保留”的完整升级安装验收此前由用户选择稍后进行；本轮核验发行、公开下载、签名和旧版本检查更新。Windows Authenticode 签名仍未配置。
