//! 内部AI通过Hub读取目录、调用LLM形成计划；无执行/确认权限。
use plugin_sdk::contracts::agent::AskRequest;
use plugin_sdk::contracts::llm::{ChatMessage, ChatRequest, LlmClient, Tier};
use plugin_sdk::contracts::operations::{
    contains_secret, OperationsClient, PrepareRequest, SERVICE,
};
use plugin_sdk::ServiceError;
use serde_json::{json, Value};
pub(crate) async fn ask(
    ctx: &plugin_sdk::Context,
    sid: i64,
    request: &AskRequest,
) -> Option<Value> {
    if contains_secret(&request.question) {
        return Some(
            json!({"answer":"敏感信息请在确认面板安全输入框填写","operation":{"plans":[]}}),
        );
    }
    let q = request.question.to_lowercase();
    if ![
        "配置",
        "设置",
        "信源",
        "rss",
        "模型",
        "插件",
        "推送",
        "关注",
        "代理",
        "队列",
        "自启",
        "托盘",
        "mcp",
        "x账号",
        "x 账号",
        "会话重命名",
    ]
    .iter()
    .any(|k| q.contains(k))
    {
        return None;
    }
    let result=async{
        let client=OperationsClient(ctx.service(SERVICE)); let snapshot=client.inspect().await?; let catalogue=client.catalogue().await?;
        let prompt=format!("你是本应用的MCP客户端。只根据用户的明确意图选择工具。普通新闻分析返回{{\"mode\":\"analysis\"}}。配置查询根据给定快照简要回答。修改只能调用清单工具创建待确认计划，不得声称已执行。不要猜测目标ID，不明确就提问。watch_assets必须提交完整assets逗号分隔字符串；修改时保留用户未要求删除的已关注项。paused/enabled使用布尔值。涉及API密钥，留给确认面板，不询问或输出密钥。最多3个工具。返回JSON:{{\"mode\":\"operation\",\"answer\":\"中文简述\",\"tools\":[{{\"name\":\"工具名称\",\"arguments\":{{}}}}]}}。清单：{}。当前只读快照：{}",serde_json::to_string(&catalogue).unwrap_or_default(),snapshot);
        let response=LlmClient(ctx.service("llm")).chat(&ChatRequest{messages:vec![ChatMessage::system(prompt),ChatMessage::user(&request.question)],tier:Tier::Large,json:true,max_tokens:Some(1800),temperature:Some(0.0),purpose:Some("app_mcp".into())}).await?;
        let value:Value=serde_json::from_str(&response.content).map_err(|_|ServiceError::internal("模型未返回有效操作计划，请重试"))?;
        if value["mode"]=="analysis"{return Ok(None)}
        let mut plans = Vec::new();
        let mut reads = Vec::new();
        if let Some(calls) = value["tools"].as_array() {
            for call in calls.iter().take(3) {
                let name = call["name"].as_str().unwrap_or("");
                let arguments = call.get("arguments").cloned().unwrap_or(json!({}));
                match name {
                    "app_inspect" => reads.push(client.inspect().await?),
                    "plan_status" => reads.push(client.status(arguments["id"].as_str().unwrap_or("")).await?),
                    _ => plans.push(client.prepare(&PrepareRequest { tool: name.into(), arguments, session_id: Some(sid) }).await?),
                }
            }
        }
        Ok::<_,ServiceError>(Some(json!({"answer":value["answer"].as_str().unwrap_or("请查看待确认操作。"),"operation":{"answer":value["answer"],"plans":plans,"reads":reads}})))
    }.await;
    match result {
        Ok(v) => v,
        Err(e) => Some(
            json!({"answer":format!("应用操作暂未执行：{}",e.message),"operation":{"answer":format!("应用操作暂未执行：{}",e.message),"plans":[]}}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_sdk::testing::{test_http_client, TestHub};
    #[tokio::test]
    async fn readonly_tool_uses_hub_status_instead_of_preparing_a_write() {
        let hub = TestHub::new();
        hub.register_fn("operations", SERVICE, |method, _| async move {
            match method.as_str() {
                "catalogue" => Ok(json!([])),
                "inspect" => Ok(json!({"watch_assets":[]})),
                "status" => Ok(json!({"id":"prior-plan","status":"cancelled"})),
                _ => panic!("readonly query must not call {method}"),
            }
        });
        hub.register_fn("llm", "llm", |method, _| async move {
            assert_eq!(method, "chat");
            Ok(json!({"model":"fixture","content":json!({"mode":"operation","answer":"查看计划状态","tools":[{"name":"plan_status","arguments":{"id":"prior-plan"}}]}).to_string()}))
        });
        let result = ask(
            &hub.context("agent", json!({}), test_http_client()),
            1,
            &AskRequest {
                question: "查询配置计划状态".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result["operation"]["reads"][0]["status"], "cancelled");
        assert!(result["operation"]["plans"].as_array().unwrap().is_empty());
    }
}
