//! `--mock` 模式用的本机模拟 OpenAI 兼容服务（随机端口）。

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn grading_json() -> String {
    r#"{"results":[
{"id":1,"score":0.95,"reason":"监管机构执法公告"},
{"id":2,"score":0.9,"reason":"交易所下架公告"},
{"id":3,"score":0.1,"reason":"KOL 市场预测"},
{"id":4,"score":0.05,"reason":"科普内容"},
{"id":5,"score":0.9,"reason":"央行利率决议超预期"}]}"#
        .to_string()
}

fn completion(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{"message": {"role": "assistant", "content": content}}],
        "usage": {"prompt_tokens": 120, "completion_tokens": 60, "total_tokens": 180}
    }))
}

pub async fn start() -> MockServer {
    let server = MockServer::start().await;
    let post = || Mock::given(method("POST")).and(path("/chat/completions"));

    // 流式：SSE 分块
    let sse = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"以太坊是\"}}]}\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"一个支持智能合约\"}}]}\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"的区块链平台。\"}}]}\n\n\
        data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":15,\"total_tokens\":35}}\n\n\
        data: [DONE]\n\n";
    post()
        .and(body_string_contains("\"stream\":true"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;

    // json_object 模式：纯 JSON
    post()
        .and(body_string_contains("json_object"))
        .respond_with(completion(&grading_json()))
        .mount(&server)
        .await;

    // 普通分级：带代码块标记和多余文字
    post()
        .and(body_string_contains("消息列表"))
        .respond_with(completion(&format!(
            "好的，结果如下：\n```json\n{}\n```\n以上。",
            grading_json()
        )))
        .mount(&server)
        .await;

    // 普通对话
    post()
        .respond_with(completion("比特币减半是指挖矿区块奖励约每四年减少一半。"))
        .mount(&server)
        .await;

    server
}
