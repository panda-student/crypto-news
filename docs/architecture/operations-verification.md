# 应用操作插件拆分验证（2026-10-23）

本轮完成本地源码与架构图更新。运行验收使用重新构建的 app serve、独立数据目录和真实 Hub；内部 Agent 的模型响应由本机 HTTP 模拟服务提供，没有调用付费模型或发送真实渠道消息。未进行新安装包发布或安装验收。

## 自动检查

- `cargo fmt --all --check`：通过。
- `cargo clippy --release --workspace --all-targets -- -D warnings`：通过，包含 desktop。
- `cargo test --release --workspace`：475 项通过，1 项既有测试忽略，0 失败。
- 最后增加只读规划、执行中断恢复断言，并调整离线提示后，`cargo test --release -p web -p agent -p operations`：89 项通过，0 失败。
- `cargo build --release -p app`：通过。验收二进制 SHA-256：`3ae707c4c25a7bf3d3043d392f0a90b8780c2c3f0e731236cfbd65eeb30f29dd`。
- 官方插件共 18 个；正常 Cargo dependencies 中插件之间直接依赖为 0（测试依赖不计）。

## 实际操作证据

| 场景 | 操作与结果 |
| --- | --- |
| 外部 MCP | 真实 app mcp stdio 初始化、获取 40 个工具，创建关注 BTC/ETH 计划；界面确认前列表为空，点击确认后才保存 |
| 内部 AI | 在首页输入“保留 BTC 和 ETH，增加关注 DOGE”；真实 Agent 通过 Hub 获取目录/快照，调用一次本机模型，再经 Hub 创建计划；点击确认前仍为 BTC/ETH，确认后为 BTC/ETH/DOGE |
| 取消与防重放 | 为 SOL 创建计划，在界面取消；币种不变，再次确认返回 HTTP 400 |
| 插件开关 | 在设置中停用“应用操作”，状态 API 返回 mcp.ok=false，MCP 返回不可用错误；界面显示服务不可用 |
| 重启与恢复 | 重启独立应用，通过可见开关重新启用插件；两条 completed 与一条 cancelled 均保留，再次确认全部拒绝，数据保持 BTC/ETH/DOGE |
| 浏览器错误 | 验收会话的 errors 输出为空 |

本地截图与机器回执位于 `target/operations-acceptance-20261023/`（忽略目录）：external-preview.png、internal-preview.png、offline.png、plugin-switch-restored.png、external-mcp.json、internal-preview.json、acceptance.json。生产用户的数据目录和设置未改动。

## 架构图

`hub-operations-v1.json` → `architecture.html`；9 项结构校验通过，0 警告。真实浏览器四个尺寸、明暗两主题检查通过，并目视复核 1440×900 浅色和 2048×1320 深色。结构、浏览器和目视回执分别为 architecture.validation.json、architecture.visual-check.json、architecture.perceptual-review.json。
