# 数据与接口

最后更新：2026-10-17（含阶段 4 T-040～T-045）

MVP 的工程结构、插件清单、事件、数据表、扩展插件协议和 HTTP 接口概览，以及效率改版新增的服务、字段和接口。细节在开发时再补。后续扩展见 `design.md` 的"后续扩展"。

## 工程结构

一个 Rust workspace，分应用层、框架层、插件层。官方插件编译进 `app`；扩展插件是插件目录下的单独可执行文件，由 hub 启动时扫描并拉起。插件只依赖框架层，不依赖应用层和其他插件的代码。

| crate | 层 | 职责 |
| --- | --- | --- |
| `app` | 应用 | 唯一官方可执行程序，子命令 `serve`、`plugin <名字>` |
| `desktop` | 应用 | Tauri 2 桌面外壳（阶段 4，可选），app 不依赖它 |
| `hub` | 框架 | 插件加载与生命周期、服务表、事件日志与投递、扩展插件进程管理 |
| `contracts` | 框架 | 官方表结构、事件、服务接口，只增不删 |
| `plugin-sdk` | 框架 | 编写插件的统一接口 |
| `store` | 插件 | SQLite、按清单建表、加密字段 |
| `llm` | 插件 | 多模型服务；OpenAI 兼容与 Anthropic 两个适配器，真流式 |
| `rulebook` | 插件 | 规则文本 |
| `auth` | 插件 | 认证与权限统一入口 |
| `web` | 插件 | HTTP 接口与网页界面 |
| `scheduler` | 插件 | 定时事件 |
| `collect` | 插件 | 调度信源抓取，写入原始消息 |
| `screen` | 插件 | 初筛分级 |
| `dispatch` | 插件 | 快讯、日报、发送队列 |
| `agent` | 插件 | 追问，提供 `agent.ask`、`agent.ask_stream` 服务 |
| `maintenance` | 插件 | 备份与清理 |
| `rss`、`okx-ann`、`binance-ann`、`x-browser` | 插件 | 信源服务；x-browser 另提供 `x-admin` |
| `wechat-ilink`、`wecom-webhook` | 插件 | 渠道服务 |

## 插件清单

每个插件有一份 `plugin.toml`，声明名称版本、提供与依赖的服务、事件、权限、自己的表、配置项。

示例（`collect`）：

```toml
name = "collect"
version = "0.1.0"

[requires]
store = "1"
source = { version = "1", all = true }  # 使用全部信源服务提供者

[provides]
item-query = "1"

[events]
publish = ["item.collected", "source.failed"]
subscribe = ["timer.tick"]
```

hub 处理依赖的情况：

| 情况 | 处理 |
| --- | --- |
| 依赖齐全 | 按依赖顺序启动 |
| 缺必需服务 | 不启动，界面提示 |
| 缺可选服务 | 照常启动 |
| 提供者崩溃 | 依赖它的插件暂停，恢复后自动恢复 |
| 服务版本不满足 | 不启动，并提示 |
| 循环依赖 | 相关插件都不启动，并报错 |

## MVP 事件

| 事件 | 发出者 | 订阅者 | 用途 |
| --- | --- | --- | --- |
| 消息入库 | collect | screen | 触发初筛 |
| 消息已分级 | screen | dispatch | 收集日报素材；事件里带 `assets`（币种代码数组）和 `direction`（可空） |
| 待推送内容 | screen（以后 merge、debate 也发） | dispatch | 紧急消息简析后推快讯 |
| 推送发出前（可拦截） | dispatch | dispatch 自带的免打扰与合并快讯（以后可由其他插件订阅） | 延后、合并或放行 |
| 定时事件 | scheduler | collect、dispatch、maintenance | 到点触发 |
| 用户发来消息（持久） | 渠道插件 | agent | 渠道收到用户消息，靠持久事件传递 |
| 渠道状态变化 | 渠道插件 | dispatch、web | 绑定、掉线、恢复；dispatch 据此得知渠道是否可用 |
| 信源异常 | collect | dispatch | 信源连续失败，提醒用户 |

## MVP 数据表

公共表所有用户共享，个人表带用户 ID；插件自己声明的表带插件名前缀。

| 表 | 用途 | 负责插件 |
| --- | --- | --- |
| `plugins` | 已安装插件及健康状态 | hub |
| `event_log` | 持久事件日志与订阅位置 | hub |
| `audit_log` | 操作日志 | hub |
| `users` | 用户，首版仅一个主人 | auth |
| `secrets` | 加密保存的密钥 | store |
| `sources` | 信源运行状态 | collect |
| `raw_items` | 原始消息 | collect |
| `screenings` | 初筛结果；`assets`（TEXT，JSON 数组字符串，缺省 NULL 视为空数组）、`direction`（TEXT，`up`/`down`/`neutral`，可空） | screen |
| `digests` | 日报记录 | dispatch |
| `<渠道名>_session` | 渠道自己的凭证、额度与会话状态 | 各渠道插件（如 wechat-ilink） |
| `outbox` | 发送队列 | dispatch |
| `usage` | 模型用量（日期、模型、用途维度的调用次数、token、失败次数；无金额） | llm |
| `providers` | 模型服务：`id`、`name`、`kind`（openai/anthropic）、`base_url`、`models`（JSON 数组）、`is_default`、`last_test`（JSON，可空）、`created_at`；密钥不在表里，存 secrets 的 `llm.key.<id>` | llm |
| `assignments` | 用途分配：`purpose`（主键）、`provider_id`、`model` | llm |
| `feeds` | RSS 订阅：`id`、`name`、`url`、`paused`、`builtin`、`created_at` | rss |
| `feed_meta` | rss 内部标记（如已播种过的内置订阅 ID） | rss |
| `x_accounts` | X 关注账号：`handle`、`paused`、`builtin`、`created_at` | x-browser |
| `item_states` | 已读与待跟进：`user_id`、`item_id`、`read_at`、`starred_at`，主键 `(user_id, item_id)` | web |
| `prefs` | 个人偏好：`user_id`、`key`、`value`，主键 `(user_id, key)`；键有 `watch_assets`（关注币种）、`last_seen_at`（上次查看动态页） | web |
| `chat_sessions` | 追问会话：`id`、`user_id`、`title`、`item_id`（兼容旧单消息会话）、`created_at`、`updated_at` | web |
| `chat_turns` | 追问的一轮问答：`id`、`session_id`、`question`、`reply_json`（`AskReply` 的 JSON）、`created_at` | web |

## 服务接口补充

### screening 1.1（screen 提供）

在 1.0 的 `get`、`list`、`review` 基础上：

- `Screening` 增加 `assets`（`Vec<String>`，大写币种代码，最多 5 个，缺省空）和 `direction`（`Direction`，`up`/`down`/`neutral`，可空）。
- 新增 `stats`：参数 `StatsQuery { since, until? }`（RFC3339 UTC，`since` 必填，非法返回 `INVALID_PARAMS`），返回 `ScreeningStats { by_day, by_source, by_asset }`，每项是 `StatsCount { key, level, count }`。
  - 只统计 `status = done` 的消息，按 `screened_at` 过滤，区间左闭右开 `[since, until)`，`until` 缺省到现在。
  - `by_day` 的 key 是 UTC 日期 `YYYY-MM-DD`，`by_source` 的 key 是信源 ID，`by_asset` 的 key 是币种代码（一条消息涉及多个币种时各计一次）；各组按 key 升序。
  - 任何调用方可读。

### llm 1.2（llm 提供）

在 1.0 的 `chat`、`usage`、`ready` 基础上：

- `chat_stream`：参数是 `chat` 的参数加调用方生成的 `stream_id`；每段输出发 ephemeral 事件 `llm.delta {stream_id, text}`；收到 ephemeral 事件 `llm.cancel {stream_id}` 后停止，返回已生成部分且 `cancelled = true`。
- `test`：参数 `provider_id`（缺省为默认服务），返回 `TestResult { ok, message, latency_ms }`，并写回 `providers.last_test`。
- 服务管理：`providers`（列表，含 `has_key`，不含密钥）、`provider_save`（`api_key` 为空表示不改，新建必填）、`provider_delete`（连带删除密钥与分配；默认服务不可删）、`set_default`、`assign`（`purpose` + `provider_id` + `model`，model 须在该服务的 models 里）。
- 用途：`screen`、`analyze`、`agent`、`digest`。
- 请求带用途且有分配时用分配；否则用默认服务（小档取第一个模型，大档取第二个，没有则第一个）。

### feed-admin 1.0（rss 提供）

`list`、`save`、`delete`、`set_paused`。url 只接受 http/https 且不能重复，name 不能为空；新订阅 id 为 `rss:` 加由 url 生成的短标识。

### source-status 1.1（collect 提供）

在 1.0 基础上新增 `retry {id}`：信源存在时清掉上次尝试时间并立即唤醒调度循环，立即返回 `{ok:true}`，不等抓取完成；不存在返回 `INVALID_PARAMS`。

### agent 1.0（agent 提供）

`ask`：界面追问，只允许 web 调用，其他调用方被拒；一次性返回完整回答；模型不可用时返回 `UNAVAILABLE`。

| 结构 | 字段 |
| --- | --- |
| `AskRequest` | `question`（最多 500 字，超出截掉）、`item_id`（可空，填了则这条消息一定放进资料）、`item_ids`（可选数组，最多 20 条，显式列表优先于旧 `item_id`；空列表启用通用检索）、`history`（`Vec<ChatTurn>`，按时间顺序，最多取最后 5 轮） |
| `ChatTurn` | `question`、`answer`（上一轮回答的结论段） |
| `AskReply` | `found`（false 表示资料里没有相关消息，此时只有 `conclusion` 有内容）、`conclusion`、`bullish`、`bearish`、`unconfirmed`（三者都是字符串数组，每条末尾带 `[n]`）、`citations` |
| `Citation` | `item_id`、`title`、`url`；序号即正文里的 `[n]`，从 1 开始 |

回答只依据已采集的消息，不做价格预测，不给投资建议。

agent 1.1 新增 `ask_stream`：参数是 `AskRequest` 加 `stream_id`，回答正文以 `llm.delta` 事件逐段发出，返回 `AskStreamReply { found, answer, citations, cancelled }`；`citations` 每项含序号 `n`、`item_id`、`title`、`url`，序号与流式正文里的 `[n]` 一致。

### dispatch 配置补充

新增布尔配置 `push_urgent`、`push_important`、`digest_enabled`（默认都为 true），与 `digest_time`、`quiet_hours` 一起通过 hub-admin `config_get/config_set` 读写。

### hub-admin 应用键

设置页和 desktop 通过 `app_get/app_set` 共享：`autostart`、`close_to_tray`、`shell`（desktop 核心启动后写入 `desktop`）、`proxy`（空字符串表示直连，重启后生效）。

## 扩展插件协议

只有扩展插件（单独进程）用；官方插件在进程内直接调用。stdio 上的 JSON-RPC 2.0，传输层是 trait。

| 方法 | 方向 | 用途 |
| --- | --- | --- |
| 握手 | hub → 插件 | 交换协议版本、清单、代理设置 |
| `configure` | hub → 插件 | 下发配置和密钥 |
| 服务调用 | 双向 | 调用别的插件的服务，hub 转给提供者 |
| 事件 | 双向 | 发出事件、投递订阅的事件、回执处理位置 |
| `log` / `metric` | 插件 → hub | 上报日志和指标，hub 统一脱敏 |
| `health.ping` | hub → 插件 | 健康检查 |
| `shutdown` | hub → 插件 | 优雅退出 |

## MVP HTTP 接口

由 `web` 插件提供，前缀 `/api/v1`，首版只监听本机，所有请求经认证与权限的统一入口。

| 分组 | 用途 |
| --- | --- |
| `/auth` | 查询当前用户 |
| `/items` | 原始消息与初筛结果 |
| `/sources` | 信源状态与试抓预览 |
| `/channels` | 渠道绑定状态与发起绑定 |
| `/outbox` | 推送记录与发送状态 |
| `/secrets` | 写入密钥，列表只返回是否已配置 |
| `/plugins` | 插件、依赖状态、权限确认、配置表单 |
| `/usage`、`/audit` | 用量与操作日志 |

效率改版新增（读用 `web.view`，写用 `web.manage`）：

| 页面 | 用途 |
| --- | --- |
| `GET /` | 动态页（原首页的插件页移到 `/plugins`） |
| `GET /feed`、`GET /feed/new?since=` | 消息流列表片段（带筛选参数和 offset）；新消息计数片段 |
| `GET /items/{id}` | 详情片段 |
| `GET /stats` | 统计页 |
| `GET /chat/{session_id}` | 对话片段 |
| `GET /settings`、`GET /plugins` | 设置入口页；插件页 |

动态页筛选参数：`asset`、`lv`（`all`/`ui`/`u`）、`dir`（`up`/`down`）、`unread=1`、`star=1`、`src`、`day`、`q`。

| 接口 | 用途 |
| --- | --- |
| `POST /api/v1/items/{id}/read` | 标为已读 |
| `POST /api/v1/items/read_all` | 全部标为已读 |
| `POST /api/v1/items/{id}/star` | 切换待跟进 |
| `POST /api/v1/items/{id}/open` | 取消息 url，只允许 http/https，由本机核心用系统默认浏览器打开；权限 `web.view` |
| `GET /api/v1/stats?days=7` | 统计数据 |
| `POST /api/v1/chat` | 提问，参数 `session_id?`、`item_id?`、`item_ids?`、`question`；无 `session_id` 时新建会话 |
| `POST /api/v1/chat/{id}/rename`、`POST /api/v1/chat/{id}/delete` | 重命名、删除会话 |
| `POST /api/v1/prefs/watch_assets` | 保存关注币种 |

阶段 4 新增与变化：

| 页面或接口 | 用途 |
| --- | --- |
| `GET /api/v1/dash` | 主界面看板数据（KPI、按天/按小时、级别分布、信源排行与下钻） |
| `POST /api/v1/chat` | 提问，存用户消息并生成 `stream_id`，返回占位片段 |
| `GET /api/v1/chat/stream/{stream_id}` | SSE：`delta` 逐段文本，`done` 带渲染后的回答与引用 |
| `POST /api/v1/chat/stream/{stream_id}/cancel` | 发布 `llm.cancel`，保存已生成部分 |
| `GET /api/v1/chat/sessions`、`GET /api/v1/chat/session/{id}` | 会话列表、单个会话 |
| `POST /api/v1/chat/undo/{token}` | 撤销刚删除的会话 |
| `GET /settings` 及 `/settings/llm`、`/settings/sources`、`/settings/push`、`/settings/plugins`、`/settings/system` | 设置页及五个分区片段与对应保存、测试、重试等操作 |
| `/plugins`、`/sources` | 重定向到 `/settings` 对应分区 |

## web 插件清单变化

- `permissions = ["store"]`，`tables = ["item_states", "prefs", "chat_sessions", "chat_turns"]`。
- `[requires]` 增加 `store = "1"`、`agent = { version = "1", optional = true }`；agent 缺失时追问显示提示，不报错。
- 原有的可选依赖（`screening`、`item-query` 等）保持。
- 阶段 4：增加可选依赖 `feed-admin = { version = "1", optional = true }`；订阅 `llm.delta`，发布 `llm.cancel`（ephemeral）。

## screen 与 agent 清单变化

- screen：`[provides]` 改为 `screening = "1.1"`；新增迁移给 `screenings` 加 `assets`、`direction`。
- agent：新增 `[provides]`，`agent = "1"`（阶段 4 升到 `1.1`，增加 `ask_stream`）；原有 `user.message` 订阅不变。
- llm：`provides llm = "1.2"`；发布 `llm.delta`（ephemeral），订阅 `llm.cancel`；表 `usage`、`providers`、`assignments`。
- rss：`provides feed-admin = "1.0"`；表 `feeds`、`feed_meta`。collect：`source-status = "1.1"`。
- x-browser：provides `source = "1.0"`、`x-admin = "1.0"`；表 `x_accounts`。
- screen：补分析标记列（含重试次数）写成 screen 自己的迁移。

## 待开发时补充

- 表字段、索引与约束
- 事件数据格式与投递语义（重试、回放、位置记录）
- 服务接口定义
- 插件清单格式细节
- 插件协议的参数、返回值结构和错误码
- HTTP 接口的请求与响应格式

## v17 当前界面接口

- `POST /api/v1/chat/context`：`{session_id?: number, items: [{id, after}]}`，保存当前资料；无会话时创建，返回会话 ID。`after` 为加入时已存在的问答轮次数，最多 20 条，不允许重复 ID。存在 `prefs` 的 `chat_context_<session id>` 键中，旧会话自动兼容 `item_id`。
- `GET /api/v1/chat/session/{id}`：返回全部 `context`（含消息元信息）及 `turns`；每轮保存 `item_ids` 快照，移出不改变旧回答的引用。
- `GET /api/v1/feed/history?before_id=<id>`：每页最多 100 条，返回 `items`、`next`、`more`；通过 item-query 的 `(collected_at,id)` 游标分页，同秒消息不漏项。
- `ItemQuery.before_id` 是可选游标，不改变已有调用的行为。
- `POST /api/v1/items/read_all` 返回实际标记的 `ids`，客户端只更新这些消息。
- 页面自动/手动刷新调用 `GET /api/v1/dash` 同步已采集数据，不强制改变信源的采集周期。

## 0.1.2 共享会话存储

- agent 迁移 2 新增 `conversation_turns`：owner、唯一 turn_key、conversation_key、channel、origin、question、reply_json、created_at。conversation_key 编码渠道/外部用户/渠道会话；按 ID 游标分页。
- `agent.conversation_turns {after}`：仅允许 web，返回最多 100 条记录，用于持久化投影补齐。
- `agent.record_conversation_turn RecordConversationTurn`：仅允许 web 写入 desktop 来源的续问，按唯一 turn_key 幂等。
- web 迁移 2 为 chat_sessions / chat_turns 增加 remote_key 及 user_id+remote_key 唯一索引；导入会话/轮次采用事务。
- web 完成桌面回答后保留 sync_pending 待同步状态，独立后台任务每 5 秒补传并拉取共享记录，不依赖页面打开。同步游标存于 prefs。新记录同时更新会话 revision，前端刷新选中的会话时保留输入和滚动位置。
- HTTP 会话列表新增 shared、updated_at、sync_error；单会话的轮次新增 origin，标明微信/应用来源。

## 0.1.3 重点事件与推送

- screening.focus 可选元数据：event_key/update_key/change/priority/basis/source_confirmed；旧数据缺失时保守展示待核实。screen迁移6新增focus，迁移7保留编号但新安装不再重置历史；迁移8新增 input_hash 索引用于精确内容复用。已完成的历史消息不自动补分析。
- GET/POST /api/v1/push/settings：enabled, scope(all/urgent/followed), assets, interval(5/10/15), effective_at；读要求web.view，写要求web.manage，经dispatch服务保存，followed启用时至少一个币种。
- dispatch迁移2：focus_settings配置、focus_receipts事件修订去重、focus_pending合并队列。只通过服务访问，web不跨插件查表。队列入账、合并落地outbox均采用SQLite事务；发送前筛掉过时材料。


- 0.1.7：channel.info 增加可选 last_received_at；/api/v1/status 汇总 channels/sources/plugins/model，每区含 ok/data/error，顶层 updated_at。分区失败不冒充正常状态，保持原插件为数据真源，不引入统一状态历史表。

- 0.1.8：dispatch 迁移3为 outbox 新增 user_requested（默认0），区分用户明确测试/重试和自动通知；停止历史通知耗尽微信额度，保留状态及抑制原因供排查。

- 0.1.9：dispatch.queue_summary 返回 total、channels 与 merging_events。total/channels 统计 pending、ready、delayed、manual；全局状态快照的 queue 分区提供相同数据，不包含消息正文。

## 0.2.1 MCP 与桌面更新

- 外部MCP使用stdio JSON-RPC（2025-06-18）；`desktop.exe mcp`通过本机回环HTTP桥接统一工具目录。支持initialize、tools/list、tools/call、ping；通知不写stdout。主应用须已运行。
- `/api/v1/mcp`是私有本机桥接，不宣称支持Streamable HTTP，拒绝带Origin的请求。所有工具调用要求web.manage，查询快照清除敏感字段。
- 查询工具app_inspect、plan_status直接读取。变更工具经Hub的app-operations服务创建operation_plans持久计划（旧prefs.mcp_plan_*经workspace-state幂等导入），GUI通过POST `/api/v1/mcp/plans/{id}/confirm`或`cancel`处理。确认串行、先持久化running、终态不可重放，有效期1小时；密钥经单次确认体传入operations，再通过Hub交给能力服务，不写计划、不交给模型。
- `/api/v1/status`新增mcp和updates分区。各插件仍持有自己的状态，web只汇总实时快照。
- GET/POST `/api/v1/prefs/watch_assets`共享已关注币种数据；网络独立路由为`/settings/network`，旧system/proxy写入路径保留兼容。
- hub-admin应用设置增加auto_update、update_action；update_status为运行时数据，桌面外壳提供idle/checking/available/downloading/installing/current/error。自动检查默认开启，每6小时检查；安装须显式请求。重启不重放旧安装请求。

### 应用操作服务拆分

- `app-operations` 1.0（operations插件）：catalogue、inspect、prepare、status；仅web允许plans、confirm、cancel。调用方由Hub绑定，限定主人身份的web/agent，Agent不能执行或取消。
- `agent` 1.2：新增operate(OperateRequest)，仅web发起；通过Hub调用llm与操作服务规划，返回Option<JSON>，普通消息交回原问答流程。
- `workspace-state` 1.0（web插件）：仅operations可调用inspect、legacy_plans以及watch_assets/item_read/item_star/items_read_all/session_rename/session_delete；不读取其他插件表。
- 必需服务依赖仍决定启动和循环错误；可选依赖是排序偏好，不因互相提供可选服务导致启动失败。服务调用时仍校验声明和可用性。
