//! 分级提示词与构造的测试消息。

use serde_json::Value;

pub struct Msg {
    pub id: u32,
    pub source: &'static str,
    pub title: &'static str,
    pub body: &'static str,
}

pub fn sample_messages() -> Vec<Msg> {
    vec![
        Msg {
            id: 1,
            source: "SEC 官网",
            title: "SEC 对某加密交易平台提起证券欺诈诉讼",
            body: "美国证券交易委员会今日宣布，已向联邦法院对某加密货币交易平台及其创始人提起诉讼，指控其未注册经营证券交易并挪用客户资金，涉及金额超过 1 亿美元。",
        },
        Msg {
            id: 2,
            source: "某交易所公告",
            title: "关于下架 XYZ 等 3 个现货交易对的公告",
            body: "本交易所决定于 UTC 时间 2026-10-20 06:00 下架 XYZ/USDT 等 3 个现货交易对，下架后将停止交易，用户可在 30 天内提现。",
        },
        Msg {
            id: 3,
            source: "某 KOL 推文",
            title: "我认为 BTC 下周必破 10 万",
            body: "根据我的技术分析，BTC 下周极有可能突破 10 万美元，大家可以提前布局，欢迎关注我的频道获取更多信号。",
        },
        Msg {
            id: 4,
            source: "某媒体",
            title: "一文读懂什么是 Layer2",
            body: "Layer2 是建立在以太坊等主链之上的扩容方案，本文将从原理、分类和代表项目三个方面为你科普。",
        },
        Msg {
            id: 5,
            source: "美联储官网",
            title: "美联储宣布将联邦基金利率下调 25 个基点",
            body: "联邦公开市场委员会今日决定将联邦基金利率目标区间下调 25 个基点，高于市场预期的维持不变，并表示将继续评估后续数据。",
        },
    ]
}

pub fn grading_prompt(msgs: &[Msg]) -> String {
    let mut s = String::from(
        "你是加密货币行业的消息面分析师，负责对下列消息做初筛打分。\n\
对每条消息给出 0 到 1 的重要性分数 score 和一句中文理由 reason。\n\n\
判定维度：\n\
1. 发布方是否权威（监管机构、交易所官方、央行等）\n\
2. 是否有明确的动作（执法、上线、下架、加息降息等），而不是观点\n\
3. 数据或结果是否超出市场预期\n\
4. 是否涉及政策变化\n\n\
严格排除（一律给 0.3 以下）：分析评论、市场预测、科普、无依据猜测、KOL 观点、营销推广。\n\n\
只输出 JSON，不要任何其他文字，格式：\n\
{\"results\":[{\"id\":1,\"score\":0.85,\"reason\":\"...\"}]}\n\
每条消息必须有且仅有一个结果，id 与输入编号一致。\n\n消息列表：\n",
    );
    for m in msgs {
        s.push_str(&format!(
            "\n[{}] 来源：{}\n标题：{}\n正文：{}\n",
            m.id, m.source, m.title, m.body
        ));
    }
    s
}

/// 校验分级结果，返回结果条数。
pub fn check_grading(v: &Value, expected: usize) -> Result<usize, String> {
    let arr = v["results"].as_array().ok_or("缺少 results 数组")?;
    for r in arr {
        let id = r["id"].as_u64().ok_or("结果缺少整数 id")?;
        let score = r["score"]
            .as_f64()
            .ok_or(format!("id {id} 缺少数值 score"))?;
        if !(0.0..=1.0).contains(&score) {
            return Err(format!("id {id} 的 score {score} 不在 0~1"));
        }
        if r["reason"].as_str().is_none_or(str::is_empty) {
            return Err(format!("id {id} 缺少 reason"));
        }
    }
    if arr.len() != expected {
        return Err(format!("期望 {expected} 条，实际 {} 条", arr.len()));
    }
    Ok(arr.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prompt_contains_all_messages() {
        let msgs = sample_messages();
        let p = grading_prompt(&msgs);
        for m in &msgs {
            assert!(p.contains(m.title));
        }
        assert!(p.contains("严格排除"));
    }

    #[test]
    fn check_grading_cases() {
        let ok = json!({"results":[{"id":1,"score":0.5,"reason":"x"}]});
        assert_eq!(check_grading(&ok, 1), Ok(1));
        assert!(check_grading(&ok, 2).is_err());
        let bad = json!({"results":[{"id":1,"score":1.5,"reason":"x"}]});
        assert!(check_grading(&bad, 1).is_err());
    }
}
