# iLink（微信 ClawBot）协议笔记

本文记录 `ilink` crate 实现的协议部分。来源是对 wechat-ilink 0.6.0（MIT）行为的整理，用自己的话重写，不含媒体（CDN、AES）部分。所有主机地址由调用方配置，库内没有写死的主机。

## 通用请求头

所有请求都带：

| 头 | 值 |
| --- | --- |
| `iLink-App-Id` | `bot` |
| `iLink-App-ClientVersion` | 十进制整数，版本号 `M.N.P` 打包为 `M<<16 | N<<8 | P`；默认 `1536`（0.6.0） |
| `SKRouteTag` | 可选，配置了才发 |

已登录的 POST 请求再加：

| 头 | 值 |
| --- | --- |
| `Content-Type` | `application/json` |
| `AuthorizationType` | `ilink_bot_token` |
| `Authorization` | `Bearer <bot_token>` |
| `X-WECHAT-UIN` | 随机 u32 的十进制字符串再做 base64，每次请求重新生成 |

每个 POST 的 JSON 体都带 `base_info: {"channel_version": "0.6.0", "bot_agent": "OpenClaw"}`。

## 端点

接口前缀都是 `/ilink/bot/`。

| 用途 | 方法 | 主机 | 路径 | 超时 |
| --- | --- | --- | --- | --- |
| 取二维码 | POST | 二维码主机（配置） | `get_bot_qrcode?bot_type=3` | 15 s |
| 轮询扫码状态 | GET | 二维码主机，遇重定向换主机 | `get_qrcode_status?qrcode=..[&verify_code=..]` | 35 s |
| 收消息（长轮询） | POST | 凭证里的 `baseUrl` | `getupdates` | 45 s |
| 发消息 | POST | 凭证里的 `baseUrl` | `sendmessage` | 15 s |

## 登录流程

1. `get_bot_qrcode`，体 `{"local_token_list": []}`，返回 `qrcode`（轮询用的标识）和 `qrcode_img_content`（二维码内容，给用户扫）。
2. 循环 GET `get_qrcode_status`。字段：`status`、`bot_token`、`ilink_bot_id`、`ilink_user_id`、`baseurl`、`redirect_host`。
3. `status` 取值：
   - `wait`：等待扫码。客户端超时（35 s 长轮询到期）也按 `wait` 处理。
   - `scaned`：已扫码，等手机确认。
   - `confirmed`：成功，带 `bot_token`、`ilink_bot_id`（账号 ID）、`ilink_user_id`（用户 ID）、`baseurl`（之后收发消息的主机）。
   - `expired`：二维码过期，需重新取。
   - `need_verifycode`：需要手机上的验证码，下一次轮询带 `verify_code`。
   - `verify_code_blocked`、`binded_redirect`：被服务端拒绝。
   - `scaned_but_redirect`：把后续轮询主机换成 `https://<redirect_host>`。
4. 登录得到的凭证（token、baseUrl、账号 ID、用户 ID）由调用方保存。库本身无状态，提供 JSON 导出和恢复。

## 收消息：长轮询与游标

POST `getupdates`，体 `{"get_updates_buf": "<游标>", "base_info": ..}`。首次游标为空串。

返回 `msgs`（消息数组）和新的 `get_updates_buf`。游标处理规则：返回的游标非空且与旧值不同才推进，否则保持旧值。长轮询超时（45 s）视为空结果，游标不变。调用方负责持久化游标，崩溃重启后用旧游标继续，可能重复收到消息，需自行去重。

消息字段（用到的部分）：`message_id`（可能是数字或字符串，库统一成字符串）、`from_user_id`、`message_type`（1 为用户发来，2 为机器人发出）、`context_token`、`item_list`（`type == 1` 为文本，内容在 `text_item.text`）。

## 发文本与 context_token

POST `sendmessage`：

```json
{
  "msg": {
    "from_user_id": "",
    "to_user_id": "<用户 ID>",
    "client_id": "<客户端生成的唯一 ID>",
    "message_type": 2,
    "message_state": 2,
    "context_token": "<最近一次收到的 context_token>",
    "item_list": [{"type": 1, "msg_id": "<同 client_id>", "text_item": {"text": "..."}}]
  },
  "base_info": {..}
}
```

`context_token` 来自用户发给机器人的消息，是服务端允许机器人回复该用户的会话凭证。没有用户先发消息就没有 context_token，无法主动推送。同一个 token 能连续发送有限条数（超出见下文 -2），并且有时效，过期也会被拒绝。库不保存 token，调用方用 `Context`（账号、用户 ID、token、观察时间、来源消息 ID）持久化。

纯文本发送不需要任何加密。AES 只用于 CDN 媒体文件，本库不涉及。

## 错误码

响应体里 `errcode`（优先）或 `ret` 非零表示失败，附 `errmsg`。

| 码 | 含义 | 库里的错误 |
| --- | --- | --- |
| `-14` | 登录凭证（token）过期，需重新扫码 | `CredentialExpired` |
| `-2` | 频率或配额限制（如同一 context_token 发送条数用尽） | `RateLimited` |
| 其他非零 | 协议错误 | `Api` |

非 2xx 的 HTTP 状态：先尝试解析响应体里的错误码，解析不到再返回 `HttpStatus`。响应体不是合法 JSON 返回 `Decode`。网络层错误返回 `Network`，并去掉 URL，避免泄露路径里的参数。库不做重试和退避，由调用方决定。

## 保密

`Credentials`、`Context`、`Message` 的 `Debug` 输出隐藏 token；错误的 Display 和 Debug 不含 token。
