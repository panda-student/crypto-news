use plugin_sdk::contracts::ServiceError;
use serde_json::{json, Value};
pub(crate) struct Tool {
    pub(crate) name: &'static str,
    pub(crate) title: &'static str,
    pub(crate) fields: &'static str,
    pub(crate) secret: &'static str,
}
macro_rules! tool {
    ($n:literal,$t:literal,$f:literal) => {
        Tool {
            name: $n,
            title: $t,
            fields: $f,
            secret: "",
        }
    };
    ($n:literal,$t:literal,$f:literal,$s:literal) => {
        Tool {
            name: $n,
            title: $t,
            fields: $f,
            secret: $s,
        }
    };
}
pub(crate) const TOOLS: &[Tool] = &[
    tool!("source_save", "添加或编辑RSS信源", "id,name,url"),
    tool!("source_pause", "暂停或恢复信源", "id,paused"),
    tool!("source_delete", "删除RSS信源", "id"),
    tool!("source_retry", "立即重新采集信源", "id"),
    tool!("x_add", "关注X账号", "handle"),
    tool!("x_pause", "暂停或恢复X账号", "handle,paused"),
    tool!("x_delete", "取消关注X账号", "handle"),
    tool!("x_login", "打开X登录窗口", ""),
    tool!("x_open", "打开X首页", ""),
    tool!(
        "model_save",
        "添加或编辑模型服务",
        "id,name,kind,base_url,models",
        "api_key"
    ),
    tool!("model_assign", "分配模型用途", "purpose,provider_id,model"),
    tool!("model_default", "设置默认模型服务", "id"),
    tool!("model_test", "测试模型连接", "id"),
    tool!("model_delete", "删除模型服务", "id"),
    tool!("plugin_enabled", "启停插件", "name,enabled"),
    tool!("plugin_restart", "重启插件", "name"),
    tool!("plugin_grant", "确认插件权限", "name"),
    tool!(
        "plugin_config",
        "配置插件（fields填写配置键值）",
        "name,fields"
    ),
    tool!("watch_assets", "保存完整的关注币种列表", "assets", ""),
    tool!(
        "push_settings",
        "保存推送范围及合并间隔",
        "enabled,scope,assets,interval",
        ""
    ),
    tool!("channel_test", "发送渠道测试消息（占用真实额度）", "id"),
    tool!("channel_bind", "绑定微信渠道", "id"),
    tool!("webhook_save", "设置企业微信Webhook", "", "value"),
    tool!("outbox_retry", "重试发送队列任务", "id"),
    tool!("proxy_save", "设置网络代理或直连", "mode,url"),
    tool!("proxy_test", "测试网络连接", ""),
    tool!(
        "app_settings",
        "设置开机自启和关闭到托盘",
        "autostart,close_to_tray"
    ),
    tool!(
        "update_action",
        "检查或安装签名更新（action为check/install）",
        "action"
    ),
    tool!("auto_update", "开关自动检查更新", "enabled"),
    tool!("secret_save", "设置应用密钥（不返回密钥）", "key", "value"),
    tool!("secret_delete", "删除应用密钥", "key", ""),
    tool!("rules_save", "更新消息分析规则", "text", ""),
    tool!("item_read", "标记消息已读", "id", ""),
    tool!("item_star", "切换消息关注状态", "id", ""),
    tool!("item_level", "设置消息级别", "id,level", ""),
    tool!("items_read_all", "将默认筛选下的全部消息标为已读", "", ""),
    tool!("session_rename", "重命名会话", "id,title", ""),
    tool!("session_delete", "删除会话", "id", ""),
];

pub(crate) fn spec(name: &str) -> Result<&'static Tool, ServiceError> {
    TOOLS
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| ServiceError::invalid_params("未知MCP工具"))
}
pub(crate) use plugin_sdk::contracts::operations::contains_secret;
pub(crate) fn validate(tool: &Tool, args: &Value) -> Result<(), ServiceError> {
    let obj = args
        .as_object()
        .ok_or_else(|| ServiceError::invalid_params("工具参数必须是对象"))?;
    if obj.len() > 30 || args.to_string().len() > 16_000 {
        return Err(ServiceError::invalid_params("操作参数过长"));
    }
    for (key, value) in obj {
        if !tool.fields.split(',').any(|f| f == key) {
            return Err(ServiceError::invalid_params(format!("不支持的参数 {key}")));
        }
        if contains_secret(&value.to_string()) {
            return Err(ServiceError::invalid_params(
                "敏感信息请在应用确认面板的安全输入框填写",
            ));
        }
    }
    Ok(())
}
pub(crate) fn catalogue() -> Vec<Value> {
    let mut out = vec![
        json!({"name":"app_inspect","description":"读取配置与状态，密钥仅返回是否已设置","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"plan_status","description":"读取计划状态；pending须由用户在应用AI与MCP页面确认，外部AI不能自行授权","inputSchema":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ];
    for t in TOOLS {
        let properties: serde_json::Map<String, Value> = t
            .fields
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|f| (f.into(), json!({"description":f})))
            .collect();
        out.push(json!({"name":t.name,"description":format!("{}。只创建待确认计划，不立即执行。{}",t.title,if t.secret.is_empty(){""}else{"密钥由用户在应用确认面板填写，不要传给AI。"}),"inputSchema":{"type":"object","properties":properties,"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false}}));
    }
    out
}
