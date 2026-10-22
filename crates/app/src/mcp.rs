//! MCP stdio transport: stdout contains JSON-RPC only, all tools live in the web core.
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

pub async fn run(data_dir: Option<PathBuf>) -> i32 {
    let endpoint = (|| {
        let dir = data_dir.map(Ok).unwrap_or_else(crate::default_data_dir)?;
        let text = crate::prepare_data_dir(&dir)?;
        let config = crate::parse_config(&text)?;
        Ok::<_, crate::AppError>(format!("http://{}/api/v1/mcp", config.listen))
    })();
    let endpoint = match endpoint {
        Ok(v) => v,
        Err(e) => {
            eprintln!("MCP配置不可用：{e}");
            return 1;
        }
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(_) => return 1,
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(_) => return 1,
        };
        if line.len() > 1_048_576 {
            return 1;
        }
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                let _ = writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Invalid JSON"}})
                );
                let _ = output.flush();
                continue;
            }
        };
        let id = request.get("id").cloned();
        let result = match client.post(&endpoint).json(&request).send().await {
            Ok(r) if r.status().is_success() && id.is_some() => r.json::<Value>().await.ok(),
            _ => None,
        };
        if let Some(id) = id {
            let response=result.unwrap_or_else(||json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"应用未运行或MCP暂不可用，请先启动消息面分析。"}}));
            if writeln!(output, "{response}").is_err() || output.flush().is_err() {
                return 1;
            }
        }
    }
}
