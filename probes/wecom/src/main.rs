//! 企业微信群机器人消息长度实测。用法：先复制 config.example.toml 为 config.toml 并填写，
//! 再运行 `cargo run --manifest-path probes/wecom/Cargo.toml`。

use std::path::PathBuf;
use std::time::Duration;

use probe_wecom::{build_message, redact, split_batches};
use serde::Deserialize;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SIZES: [usize; 6] = [4000, 4090, 4096, 4097, 4200, 5000];
const BATCH_LIMIT: usize = 4000;
const MIXED_TOTAL: usize = 9000;

/// 模拟服务认定"超长"的字节数（企业微信官方说明 markdown 上限 4096）。
const MOCK_LIMIT: usize = 4096;
/// 模拟服务返回的超长错误码。待核实：未查到企业微信群机器人超长的确切错误码，暂用 40058。
const MOCK_TOO_LONG_ERRCODE: i64 = 40058;

/// 模拟的企业微信 webhook：内容超过 4096 字节返回超长错误码，否则返回 errcode 0。
struct MockWecom;

impl Respond for MockWecom {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let kind = body.get("msgtype").and_then(Value::as_str).unwrap_or("");
        let len = body
            .get(kind)
            .and_then(|v| v.get("content"))
            .and_then(Value::as_str)
            .map_or(0, str::len);
        if len > MOCK_LIMIT {
            ResponseTemplate::new(200).set_body_json(
                json!({"errcode": MOCK_TOO_LONG_ERRCODE, "errmsg": "content too long (mock)"}),
            )
        } else {
            ResponseTemplate::new(200).set_body_json(json!({"errcode": 0, "errmsg": "ok"}))
        }
    }
}

#[derive(Deserialize)]
struct Config {
    webhook_url: String,
    #[serde(default = "default_proxy")]
    proxy: String,
    #[serde(default = "default_interval")]
    interval_secs: u64,
}

fn default_proxy() -> String {
    "http://127.0.0.1:7892".into()
}
fn default_interval() -> u64 {
    4
}

#[derive(Debug, thiserror::Error)]
enum ProbeError {
    #[error("{0}")]
    Config(String),
    #[error("构造 HTTP 客户端失败: {0}")]
    Client(String),
}

struct Record {
    label: String,
    bytes: usize,
    status: String,
    errcode: String,
    errmsg: String,
}

fn config_path() -> PathBuf {
    std::env::args_os()
        .skip(1)
        .find(|a| a != "--mock" && a != "sample")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config.toml"))
}

fn load_config() -> Result<Config, ProbeError> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).map_err(|_| {
        ProbeError::Config(format!(
            "找不到 {}。请复制同目录的 config.example.toml 为 config.toml，并填写 webhook_url（含 key）。",
            path.display()
        ))
    })?;
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| ProbeError::Config(format!("config.toml 格式错误: {}", e.message())))?;
    if cfg.webhook_url.trim().is_empty() {
        return Err(ProbeError::Config(
            "config.toml 里的 webhook_url 还是空的，请填写。".into(),
        ));
    }
    Ok(cfg)
}

fn payload(kind: &str, content: &str) -> Value {
    json!({ "msgtype": kind, kind: { "content": content } })
}

async fn send(
    client: &reqwest::Client,
    cfg: &Config,
    kind: &str,
    label: &str,
    content: &str,
) -> Record {
    let mut rec = Record {
        label: label.to_string(),
        bytes: content.len(),
        status: "-".into(),
        errcode: "-".into(),
        errmsg: "-".into(),
    };
    match client
        .post(&cfg.webhook_url)
        .json(&payload(kind, content))
        .send()
        .await
    {
        Ok(resp) => {
            rec.status = resp.status().as_u16().to_string();
            match resp.text().await {
                Ok(body) => match serde_json::from_str::<Value>(&body) {
                    Ok(v) => {
                        rec.errcode = v
                            .get("errcode")
                            .map(|x| x.to_string())
                            .unwrap_or_else(|| "-".into());
                        let msg = v.get("errmsg").and_then(|x| x.as_str()).unwrap_or("-");
                        rec.errmsg = redact(msg, &cfg.webhook_url);
                    }
                    Err(_) => {
                        rec.errmsg = redact(&format!("非 JSON 响应: {body}"), &cfg.webhook_url)
                    }
                },
                Err(e) => rec.errmsg = redact(&e.without_url().to_string(), &cfg.webhook_url),
            }
        }
        Err(e) => {
            rec.errmsg = redact(&format!("请求失败: {}", e.without_url()), &cfg.webhook_url);
        }
    }
    println!(
        "[{}] {} 字节 -> HTTP {} errcode={} errmsg={}",
        rec.label, rec.bytes, rec.status, rec.errcode, rec.errmsg
    );
    rec
}

/// 发送 1 条 markdown 格式样例，返回是否 errcode 0。
async fn run_sample(cfg: &Config, mock: bool) -> Result<bool, ProbeError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30));
    if !mock {
        let proxy =
            reqwest::Proxy::all(&cfg.proxy).map_err(|e| ProbeError::Client(e.to_string()))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|e| ProbeError::Client(e.without_url().to_string()))?;
    let content = probe_wecom::sample_markdown();
    println!(
        "发送格式样例（markdown，{} 字节）：\n{content}",
        content.len()
    );
    let rec = send(&client, cfg, "markdown", "sample", &content).await;
    Ok(rec.errcode == "0")
}

fn mixed_items(total: usize) -> Vec<String> {
    let mut items = Vec::new();
    let mut sum = 0;
    let mut i = 0;
    while sum < total {
        let item = format!(
            "{i:02}. Binance will list TOKEN{i} 币安将上线新币 TOKEN{i}，交易对 TOKEN{i}/USDT，开放充值时间见公告。"
        );
        sum += item.len() + 1;
        items.push(item);
        i += 1;
    }
    items
}

#[tokio::main]
async fn main() {
    let mock = std::env::args().any(|a| a == "--mock");
    let server = if mock {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(MockWecom)
            .mount(&server)
            .await;
        Some(server)
    } else {
        None
    };
    let cfg = match &server {
        // 假 key，仅用于演示脱敏
        Some(s) => Config {
            webhook_url: format!("{}/cgi-bin/webhook/send?key=fake-key-0000-demo", s.uri()),
            proxy: String::new(),
            interval_secs: 0,
        },
        None => match load_config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
        },
    };
    if std::env::args().any(|a| a == "sample") {
        let code = match run_sample(&cfg, mock).await {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(e) => {
                eprintln!("{e}");
                1
            }
        };
        std::process::exit(code);
    }
    if let Err(e) = run(cfg, mock).await {
        eprintln!("{e}");
        std::process::exit(1);
    }
    if let Some(server) = &server {
        // 模拟服务记录了收到的每条消息
        let reqs = server.received_requests().await.unwrap_or_default();
        println!(
            "
模拟服务共记录 {} 条消息，各条内容字节数：",
            reqs.len()
        );
        for r in &reqs {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            let kind = v.get("msgtype").and_then(Value::as_str).unwrap_or("?");
            let n = v
                .get(kind)
                .and_then(|c| c.get("content"))
                .and_then(Value::as_str)
                .map_or(0, str::len);
            println!("  {kind} {n}");
        }
    }
}

async fn run(cfg: Config, mock: bool) -> Result<(), ProbeError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30));
    if mock {
        // 唯一的例外：mock 模式请求的是本机随机端口上的模拟服务，不走代理，
        // 也不会访问外网。真实模式必须走配置里的代理。
        println!("MOCK 模式：请求本机模拟服务，不走代理");
    } else {
        let proxy =
            reqwest::Proxy::all(&cfg.proxy).map_err(|e| ProbeError::Client(e.to_string()))?;
        builder = builder.proxy(proxy);
    }
    println!(
        "webhook（已脱敏）: {}",
        redact(&cfg.webhook_url, &cfg.webhook_url)
    );
    let client = builder
        .build()
        .map_err(|e| ProbeError::Client(e.without_url().to_string()))?;
    let gap = Duration::from_secs(cfg.interval_secs);

    let mut records: Vec<Record> = Vec::new();
    for kind in ["markdown", "text"] {
        for n in SIZES {
            let Some(msg) = build_message(n) else {
                continue;
            };
            records.push(send(&client, &cfg, kind, &format!("{kind}-{n}"), &msg).await);
            tokio::time::sleep(gap).await;
        }
    }

    let items = mixed_items(MIXED_TOTAL);
    let total: usize = items.iter().map(|s| s.len() + 1).sum();
    let batches = split_batches(&items, BATCH_LIMIT);
    println!(
        "混合中英文约 {total} 字节，按 {BATCH_LIMIT} 字节切成 {} 批",
        batches.len()
    );
    for (i, b) in batches.iter().enumerate() {
        let label = format!("split-{}/{}", i + 1, batches.len());
        records.push(send(&client, &cfg, "markdown", &label, b).await);
        tokio::time::sleep(gap).await;
    }

    if mock {
        println!("\n## MOCK 结果（模拟服务，非真实企业微信）");
    }
    println!("\n| 类型 | 字节数 | HTTP | errcode | errmsg |\n|---|---|---|---|---|");
    for r in &records {
        println!(
            "| {} | {} | {} | {} | {} |",
            r.label, r.bytes, r.status, r.errcode, r.errmsg
        );
    }
    println!(
        "\n请到群里人工核对：\n\
         1. 每条消息开头的 [N字节] 与结尾的 [END-N] 是否都在；缺结尾标记说明被截断，据此判断 markdown 与 text 各自的上限。\n\
         2. 超长时看上表：errcode 非 0 为报错拒收，errcode 为 0 但群里缺结尾标记为被截断。\n\
         3. 拆分的几批是否都以 \"第 N/M 批\" 开头、完整送达、没有乱码。"
    );
    Ok(())
}
