# 插件边界与单一职责核对

结论：上一版图把独立插件合成了能力组，不能据此判断它们在同一 crate。本轮已将 web 的应用操作治理提取为 operations，AI 规划归入 Agent；其余插件在本轮核对范围内未发现必须继续拆分的明确职责冲突。

本次核对范围是 app 的运行时登记、18 份官方插件清单、Cargo 正常依赖与关键处理路径，不是对全部函数的逐行审计。

## 原图分组与实际代码

| 原图的归组 | 实际边界 | 结论 |
| --- | --- | --- |
| 采集与各信源 | collect、rss、x-browser、okx-ann、binance-ann 是 5 个插件 | collect 编排采集；各适配器提供 source 服务。原图归组不是同一份插件代码 |
| 初筛、模型、规则 | screen、llm、rulebook 是 3 个插件 | screen 决定消息级别和影响；llm 负责模型网关；rulebook 管规则 |
| 追问与会话 | agent 提供问答与共享会话，web 提供桌面交互与本地会话记录 | 是会话能力的归组，不能据此把问答服务和界面会话记录当成同一个模块 |
| 推送与渠道 | dispatch、wechat-ilink、wecom-webhook 是 3 个插件 | dispatch 编排通知和队列，各渠道负责自己的协议适配与状态 |
| 界面与 MCP | web管交互/协议、app管stdio桥；operations管计划，Agent管模型规划 | 本次完成实际代码拆分；不是只改图名 |

## 单一职责的判定

一个能力可以包含多个函数和流程环节。判断重点是它们是否服务于同一个业务职责、是否会因为不同的原因独立变化，而不是要求每个插件只做一个动作。

- **screen 的初筛与重点简析**：目前是同一次消息评估的输出，包含级别、方向、证据、摘要与影响。合并模型请求是此前确认的成本优化，仍由 screen 负责消息评估，通用模型调用留在 llm。
- **collect 的调度、去重、入库与来源健康**：围绕采集生命周期，具有内聚性；信源协议实现放在独立 source 插件。
- **dispatch 的选择、合并、排队与重试**：属于推送编排；微信和企业微信的协议实现已经分离。快讯、日报等策略较多，后续可按独立变化的业务需求继续评审，不需要因函数数量多立即拆插件。
- **agent 的检索、生成回答、共享会话和渠道问答编排**：围绕会话问答能力，边界较大但具有共同目标；现在包含内部AI操作规划，只能经Hub创建待确认计划，不拥有配置执行权。

## 本轮已实现的拆分

| 能力 | 所属代码 | Hub 服务 / 边界 |
| --- | --- | --- |
| 工具目录、持久计划、确认、取消、过期、防重放 | `operations/src/catalogue.rs`、`lib.rs` | `app-operations`，独立插件；计划存 operation_plans |
| 具体操作编排 | `operations/src/execute.rs` | 调用 feed-admin、x-admin、llm、dispatch、secrets、rulebook、hub-admin 等；无HTTP回调 |
| 内部AI理解与规划 | `agent/src/operations.rs` | 通过Hub调用 llm 和 app-operations；AI不能自行确认 |
| 外部MCP协议与确认界面 | `web/src/mcp.rs`、`app/src/mcp.rs` | stdio/本机HTTP只转换协议与展示 |
| 已关注币种、已读/收藏、本地会话 | `web/src/workspace.rs`、`data.rs` | workspace-state仅访问web自己的表，与GUI复用helper |

旧 web.prefs 内的 mcp_plan_* 通过 workspace-state 导出，由 operations 幂等导入；既有终态不会被旧数据覆盖。执行先保存 running，再发起副作用；中断状态不自动重试。计划一小时有效，秘密只在用户确认时传入，不保存到计划或交给模型。

`Agent → Hub → llm` 已在主图明确画出；实际代码使用 LlmClient，依赖为服务契约，不是插件间Rust依赖。内部AI规划和消息问答都由Agent请求模型网关。

权限扩展仅让主人身份 web / operations 调用原管理方法；secrets.get仍只允许读取调用插件所属密钥，maintenance专用备份/删除权限保持独立。外部MCP工具目录不暴露confirm，operations也拒绝Agent确认。

## 全部官方插件清单

下列各行均对应 `crates/<插件名>/plugin.toml` 与独立 crate，由 `crates/app/src/lib.rs` 的 officials 列表登记。

| 插件 | 当前主要能力 | 本轮边界判断 |
| --- | --- | --- |
| auth | 本机认证与授权 | 安全能力，首版恒为主人 |
| secrets | 密钥加密读写与主密钥获取 | 密钥能力；密文经 store 保存，主密钥来自系统钥匙串 |
| scheduler | 定时任务与 timer.tick | 定时能力 |
| collect | 调用 source、去重入库、来源健康、消息查询 | 采集编排，具有内聚性 |
| rss | RSS 抓取与订阅配置 | RSS 协议适配 |
| okx-ann | OKX 公告抓取 | 独立信源适配 |
| binance-ann | Binance 公告抓取 | 独立信源适配 |
| x-browser | 登录资料、账号管理与 X 浏览器采集 | X 采集适配 |
| llm | 模型协议、服务商、用途分配、流式调用、用量 | 通用模型网关，不负责消息业务判断 |
| rulebook | 分析规则保存与读取 | 规则能力 |
| screen | 消息初筛、方向、重点简析与分析结果 | 消息评估能力 |
| dispatch | 通知策略、合并、outbox、发送重试 | 推送编排；不同策略未来可继续评审 |
| wecom-webhook | 企业微信机器人发送、额度与状态 | 渠道协议适配 |
| wechat-ilink | 微信绑定、收发、会话窗口、额度与状态 | 渠道协议适配 |
| agent | 材料检索、问答、共享会话、渠道问答编排、内部AI操作规划 | 模型代理能力，执行权限委托operations并需用户确认 |
| maintenance | 备份与过期数据清理 | 数据维护能力 |
| operations | 操作目录、计划治理、确认及能力调用编排 | 单独的应用操作能力，无界面路由与模型推理 |
| web | 页面、HTTP/SSE、设置交互、MCP适配、界面数据服务 | 操作治理已移出，保留呈现和界面数据所有权 |

不属于官方插件的关键模块：desktop 是桌面外壳，app 是组装入口，hub 是运行时，contracts/plugin-sdk 是契约与 SDK，store 是 app 注入 Hub 的存储实现，ilink 是协议库。

Cargo 正常依赖检查：上述 18 个插件之间没有直接的插件 crate 依赖。这只能证明编译依赖边界，不能单独证明业务职责已经单一。

## 复核边界

本次核对不是对每个函数的形式化证明。保留 screen 的初筛与重点简析合并，避免重复模型调用；collect 的采集生命周期、dispatch 的通知生命周期、maintenance 的数据维护各自内聚。Agent 在渠道回复临时失败后重投可能重新调用模型，是既有的成本优化机会，本轮未混入额外重构。
