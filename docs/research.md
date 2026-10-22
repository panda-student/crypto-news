# 调研记录

2026-10-09 基于公开资料核实，尚未真机测试。结论：微信 ClawBot 不能随时主动推送，推送设计已按此调整；X 官方 API 成本可控，作为浏览器自动化的备选。

## 微信 ClawBot

| 事项 | 结论 | 对设计的影响 |
| --- | --- | --- |
| 主动推送 | 有条件可行：发送必须带上用户最近一次发来消息时下发的会话凭证，凭证约 24 小时失效；用户 24 小时内没给机器人发过消息，推送会被拒绝 | 需要保活机制和备用推送渠道 |
| 单次配额 | 每收到用户一条消息，最多可回复或推送约 10 条 | 一天内推送条数有上限，需要合并推送 |
| 频率限制 | 短时间连发会触发服务端限流，之后几分钟内全部发送失败；实测 34 秒 18 条会触发，170 秒 10 条安全 | 发送队列要限速 |
| 群聊 | 不支持，只能一对一 | 与原设计一致 |
| 多账号绑定 | SDK 把凭证交给应用自行管理，技术上一个后端可以保存多份凭证；腾讯是否允许、有无数量限制未知 | 多用户分发前仍需实测 |
| Rust 实现 | 已有 Rust 社区库（wechat-ilink、weixin-agent），非官方、实验性 | 微信插件可以用 Rust 写，不再需要 Node 边车 |

已确认的调整：日报末尾提示用户回复以续期凭证；凭证过期或配额用完时，紧急消息改走备用渠道；同一时段的多条快讯合并成一条。

2026-10-15 决定自写 iLink 协议客户端（`crates/ilink`），不用社区库 wechat-ilink：它写死 native-tls（与只用 rustls 冲突）、二维码主机写死、自带重试和本地退避，不便控制。

## X 官方 API

- 2026 年起按量计费，已无免费档和固定月费档。读取一条帖子 $0.005，读取一个用户 $0.010，按返回的条数计费。
- 同一条帖子在同一个 UTC 自然日内重复读取只计一次费。
- 每月最多读取 300 万条帖子，可以设置每月消费上限。
- 估算：白名单 100 个官方账号、平均每个账号每天 5 条，每月约 1.5 万条，约 $75。

最终决定：X 改为浏览器自动化（专用小号 + 私密列表页 + 固定脚本，AI 负责修复），官方 API 作为备选。

## 首批默认信源

| 主体类型 | 信源 | 接入方式 |
| --- | --- | --- |
| 监管 | SEC 新闻稿、诉讼公告、行政处罚 | 官方 RSS |
| 监管 | CFTC 新闻稿、执法新闻稿、讲话与证词 | 官方 RSS |
| 宏观 | 美联储新闻稿（含货币政策）、讲话与证词 | 官方 RSS |
| 交易所 | OKX 公告 | 官方公开 API，无需鉴权 |
| 交易所 | Binance 公告（上币、下架等） | 网站公开接口，未写入官方文档，可能变动 |
| 交易所 | Coinbase | 博客有反爬保护，改用其官方 X 账号 |
| 项目方 | 主要公链和项目的官方博客、GitHub 版本发布 | RSS / GitHub Atom |
| 关键人物、官方账号 | 白名单账号 | X 浏览器自动化（私密列表页） |
| 权威财经媒体 | 路透 | 官方 RSS 已于 2020 年停用，需要网页抓取或第三方聚合，待定 |
| 国家官方媒体 | 新华社、人民日报等 | 未找到可用的官方 RSS，需要网页抓取，待核实 |

## 2026-10 媒体 RSS 验证

可用：

- CoinDesk `https://www.coindesk.com/arc/outboundfeeds/rss`
- Cointelegraph `https://cointelegraph.com/rss`
- Decrypt `https://decrypt.co/feed`
- The Block `https://www.theblock.co/rss.xml`
- PANews `https://www.panewslab.com/rss.xml`
- 吴说 `https://www.wublock123.com/feed`
- Bitcoin Magazine `https://www.bitcoinmagazine.com/.rss/full/`
- Ethereum 博客 `https://blog.ethereum.org/feed.xml`

不可用：BlockBeats（无可用 RSS）、Odaily `/rss`（404）、Coinbase 博客（403）、nitter（410）、公共 RSSHub（404）。

## 阶段 0 实测结论（2026-10-15～16）

- 代理：`http://127.0.0.1:7892` 可用，所有探针都显式走代理。
- 信源：
  - SEC 新闻稿、诉讼公告、行政处罚都返回 200。User-Agent 必须带联系邮箱，否则返回 403。
  - 美联储新闻稿、讲话与证词正常。
  - CFTC 执法新闻稿、讲话与证词正常；CFTC 新闻稿会间歇被 Cloudflare 拦截，返回 403。正式采集要能重试，失败只告警，不中断整轮采集。
  - OKX 公告正常，没有 ID 字段，按 url 去重。
- Binance 公告接口（catalogId=48）：连续 5 轮、每轮间隔 10 秒都返回 200，每轮 20 条，没有触发风控。
- 模型服务（Sense）：
  - OpenAI 兼容格式，地址 `https://token.sensenova.cn/v1`，模型 `sensenova-6.8-flash-lite`。
  - 支持 `response_format=json_object`。
  - 模型带推理，普通对话约 13 秒，流式首字约 12 秒，超时要放宽。
- 企业微信群机器人：
  - markdown 单条上限 4096 字节，4097 字节返回 errcode 40058（HTTP 仍是 200）。
  - text 超长不报错，接口返回成功，但内容被静默截断：4200 字节完整，5000 字节结尾丢失。
  - 结论：只用 markdown，每条不超过 4000 字节，超出分批发送。
  - 每 4 秒发 1 条，共 15 条，没有触发限流。
- 微信 iLink：
  - 扫码登录真机成功。
  - 用户每发一条消息，机器人最多能发 10 条，包括回复和主动推送。超出返回 errcode -2，错误信息是 "prepare failed"。
  - 用户再发一条消息后，额度恢复为 10 条，两轮结果一致。
  - 正式插件的做法：合并推送；剩最后 1 条额度时提醒用户回话；额度用完后，消息排队等用户回话再补发。
  - 会话 24 小时过期：待验证。
  - 协议客户端自写，在 `crates/ilink`，不依赖 native-tls。

## 开源项目参考

2026-10-14 阅读源码整理，供阶段 2（初筛与日报）、阶段 3（推送）写任务卡时引用。参考的项目：crypto-news-bot（MIT）、TrendRadar（GPL-3.0）。鱼皮的 yupi-hot-monitor（Node，关键词热点监控）借鉴价值不大。交易所公告监控和事件聚类没有成熟开源实现。

协议约束：TrendRadar、BettaFish 是 GPL 协议，只借鉴思路，不复制代码；crypto-news-bot 是 MIT，提示词可直接借用。

### 阶段 2：初筛与日报

1. 先去重再调用模型（crypto-news-bot 去重放在模型之后，是反例）
   - 入库时零成本去重：链接规范化，去掉 utm_*、ref、source、share_*、时间戳等追踪参数，剩余参数按字母排序；有 RSS guid 优先用 guid，否则用链接；数据库唯一索引兜底。
   - 语义去重（同一件事多来源报道）两个项目都没做好，MVP 不做，由日报生成时让模型归并同类消息。
   - 不要用"主体|动作|客体"拼签名去重（模型用词不稳定），不要用英文 MiniLM 向量模型处理中文。
2. 已分析账本（来自 TrendRadar）
   - 每条消息分析成功后记账，无论判成哪一级，以后不再送模型。
   - 调用失败的批次不记账，下轮自动重试；"调用失败"与"没有命中"必须区分（TrendRadar 用返回 None 与空列表区分）。
   - 键为"消息 ID + 规则版本"；规则文件按哈希判断是否变化，小改动只重判之前没命中的，大改动全量重判（TrendRadar 以变化比例 0.6 为界）。
   - 现有 `screenings` 表已有规则版本字段，补上"失败不记账"即可，不需新表。
3. 分级提示词
   - 批量调用：每条给编号、来源、标题加正文前几百字，一批 20～50 条（TrendRadar 只给标题、一批 200 条）。
   - 模型对每条输出 0～1 分数和理由，用可配置阈值映射到四级，不让模型直接输出等级（TrendRadar 默认阈值 0.7）。
   - "严格排除"清单（crypto-news-bot）：分析评论、市场预测、科普、无依据猜测、KOL 观点、营销推广，一律判"一般"或"归档"。
   - 判定维度（crypto-news-bot）：发布方是否权威、有无明确动作、数据是否超预期、是否涉及政策变化。
   - 硬规则需自行编写：如 SEC/CFTC 执法公告、交易所下架公告直接判"紧急"。
   - 模型不可用时用紧急关键词表兜底，按整词匹配，不按子串（"sec" 会命中 "second"）。
4. 快讯提示词借用 crypto-news-bot 写法：限定字数（它用 100～180 字）、列出禁用的模糊词（"可能""据悉""分析师认为"等）、给一条反例一条正例、要求只输出 JSON；解析时去掉代码块标记，取第一个 `{` 到最后一个 `}`。
5. 日报提示词两个项目都没有可用实现，需自行设计。
6. 其他采集注意：按游标增量抓取，不要只取最近 1 小时；时间统一按 UTC 比较；请求 SEC 时带含联系邮箱的 User-Agent。

### 阶段 3：推送

1. 长消息分批（TrendRadar）
   - 先给批次头预留字节，按条目边界切分；每批标"第 N/M 批"，批间固定等待。
   - 仅单条超长时截断，且截在字符边界上，不截断多字节中文。
   - TrendRadar 企业微信按每批 4000 字节切，官方上限据称 4096 字节，未核实，放到阶段 0 实测。
2. 需自行补齐（两个项目都没做）：失败重试、限流（429）退避、中断续发——`outbox` 设计已覆盖；微信 ClawBot 每收到用户一条消息约 10 条的配额需自行建模，按配额合并推送。
3. 推送节奏：TrendRadar 用"时间窗口 + 每窗口只推一次"控制，与"日报 8:00、免打扰时段"思路一致；"只推一次"需在数据库按"日期 + 时段 + 动作"记账，重启后不重复推送。

### 不借鉴

- TrendRadar 的热榜排名权重和关键词频率统计（不适合一手信源）。
- crypto-news-bot 的签名加英文向量语义去重。
- crypto-news-bot 推送失败也标记为已发送的做法。

## 来源

- [wechat-ilink（Rust）](https://docs.rs/crate/wechat-ilink/latest/source/README.md)
- [weixin-agent（Rust）](https://docs.rs/crate/weixin-agent/0.3.0)
- [iLink 主动推送实测记录](https://adg.csdn.net/6a79866d10ee7a33f298fd7c.html)
- [weixin-ilink（Python）](https://pypi.org/project/weixin-ilink/)
- [X API 定价文档](https://docs.x.com/x-api/getting-started/pricing)
- [SEC RSS](https://www.sec.gov/about/rss-feeds)
- [CFTC RSS](https://www.cftc.gov/RSS/index.htm)
- [美联储 RSS](https://www.federalreserve.gov/feeds/feeds.htm)
- [OKX 公告接口（rust-okx）](https://docs.rs/rust-okx/0.6.4/rust_okx/api/support/struct.Support.html)
- [Binance 公告接口讨论](https://www.elitetrader.com/et/threads/systematic-binance-trading.382406/post-6069437)
- [Coinbase 博客反爬说明](https://apify.com/jungle_synthesizer/cex-token-listings-tracker)
- [路透 RSS 现状](https://feeder.co/knowledge-base/rss-content/reuters-rss-feeds/)
- [crypto-news-bot](https://github.com/chenyuan200356-droid/crypto-news-bot)
- [TrendRadar](https://github.com/sansan0/TrendRadar)
- [yupi-hot-monitor](https://github.com/liyupi/yupi-hot-monitor)
