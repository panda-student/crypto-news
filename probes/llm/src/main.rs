mod client;
mod config;
mod mock;
mod prompt;
mod util;

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use client::Llm;
use config::{Config, ConfigError, KEY_ENV};

struct Row {
    name: &'static str,
    ok: bool,
    elapsed: String,
    usage: String,
    note: String,
}

fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

fn preview(s: &str) -> String {
    s.chars().take(80).collect::<String>().replace('\n', " ")
}

fn fail(name: &'static str, note: String) -> Row {
    Row {
        name,
        ok: false,
        elapsed: "-".into(),
        usage: "-".into(),
        note,
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mock_mode = std::env::args().any(|a| a == "--mock");
    // 持有模拟服务，直到程序结束。
    let _mock_server;
    let (llm, model, proxy_desc) = if mock_mode {
        let server = mock::start().await;
        // 唯一不走代理的例外：mock 模式的目标是本机模拟服务，不是外部网络。
        let llm = match Llm::new(&server.uri(), "mock-model", "sk-mock-placeholder", None) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("初始化失败: {e}");
                return ExitCode::from(2);
            }
        };
        _mock_server = Some(server);
        (
            llm,
            "mock-model".to_string(),
            "无（MOCK，本机直连）".to_string(),
        )
    } else {
        _mock_server = None;
        let path = Path::new("probes/llm/config.toml");
        let cfg = match Config::load(path, std::env::var(KEY_ENV).ok()) {
            Ok(c) => c,
            Err(ConfigError::Missing(p)) => {
                eprintln!("找不到配置文件 {p}。");
                eprintln!(
                    "请复制 probes/llm/config.example.toml 为 probes/llm/config.toml，并填写："
                );
                eprintln!("  base_url  OpenAI 兼容服务根地址（不含 /chat/completions）");
                eprintln!("  model     模型名");
                eprintln!("  api_key   密钥（也可改用环境变量 {KEY_ENV}）");
                eprintln!("  proxy     代理，默认 http://127.0.0.1:7892");
                eprintln!("也可用 --mock 在本机模拟服务上跑通流程。请在仓库根目录运行。");
                return ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("配置有误: {e}");
                return ExitCode::from(2);
            }
        };
        let llm = match Llm::new(&cfg.base_url, &cfg.model, &cfg.api_key, Some(&cfg.proxy)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("初始化失败: {e}");
                return ExitCode::from(2);
            }
        };
        (llm, cfg.model.clone(), cfg.proxy.clone())
    };
    println!(
        "{}{}  model={}  proxy={}",
        if mock_mode { "[MOCK] " } else { "" },
        llm.describe_request(),
        model,
        proxy_desc
    );

    let mut rows = Vec::new();

    // 1. 普通对话
    match llm.chat("用一句话说明什么是比特币减半。", false).await {
        Ok(o) => {
            println!("[普通对话] {}", preview(&o.content));
            rows.push(Row {
                name: "普通对话",
                ok: true,
                elapsed: secs(o.elapsed),
                usage: o.usage.display(),
                note: String::new(),
            });
        }
        Err(e) => rows.push(fail("普通对话", e.to_string())),
    }

    // 2. 成批分级：不带 / 带 response_format
    let msgs = prompt::sample_messages();
    let grading = prompt::grading_prompt(&msgs);
    let mut json_mode_supported = "否";
    for (name, json_mode) in [
        ("成批分级（提示词约束）", false),
        ("成批分级（json_object）", true),
    ] {
        match llm.chat(&grading, json_mode).await {
            Ok(o) => {
                let parsed = util::extract_json(&o.content)
                    .map_err(|e| e.to_string())
                    .and_then(|v| prompt::check_grading(&v, msgs.len()));
                match parsed {
                    Ok(n) => {
                        if json_mode {
                            json_mode_supported = "是";
                        }
                        rows.push(Row {
                            name,
                            ok: true,
                            elapsed: secs(o.elapsed),
                            usage: o.usage.display(),
                            note: format!("解析 {n} 条"),
                        });
                    }
                    Err(why) => {
                        println!("[{name}] 原始输出: {}", preview(&o.content));
                        rows.push(Row {
                            name,
                            ok: false,
                            elapsed: secs(o.elapsed),
                            usage: o.usage.display(),
                            note: why,
                        });
                    }
                }
            }
            Err(e) => rows.push(fail(name, e.to_string())),
        }
    }

    // 3. 流式
    match llm.chat_stream("用三句话介绍以太坊。").await {
        Ok(o) => {
            let first = o.first_token.map_or("无".to_string(), secs);
            rows.push(Row {
                name: "流式输出",
                ok: !o.content.is_empty(),
                elapsed: secs(o.elapsed),
                usage: o.usage.display(),
                note: format!("首字 {first}，{} 块", o.chunks),
            });
        }
        Err(e) => rows.push(fail("流式输出", e.to_string())),
    }

    println!(
        "\n{}\n\n| 项目 | 结果 | 耗时 | 用量(提示/生成/总) | 备注 |\n|---|---|---|---|---|",
        if mock_mode {
            "## 汇总（MOCK：模拟服务，数据不代表真实服务）"
        } else {
            "## 汇总"
        }
    );
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {} |",
            r.name,
            if r.ok { "成功" } else { "失败" },
            r.elapsed,
            r.usage,
            r.note.replace('|', "/")
        );
    }
    println!("\nJSON 模式（response_format=json_object）是否支持：{json_mode_supported}");
    if rows.iter().all(|r| r.ok) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
