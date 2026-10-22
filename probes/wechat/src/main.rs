//! probe-wechat：阶段 0 实测——微信 ClawBot 主动推送（基于自写的 ilink 协议库）。
//!
//! 网络约定：真实模式下所有请求走 config.toml 的代理（reqwest 客户端强制带代理）。
//! 唯一的例外是 `--mock`：请求发往本机随机端口的模拟服务，不走代理（见 `build_http`）。

mod logging;
mod mock;
mod qr;
mod store;
mod timeutil;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ilink::{Context, Credentials, QrStatus};
use serde_json::json;
use wiremock::MockServer;

use logging::{describe, Logger};
use store::{Config, ContextRecord, Paths, Res};
use timeutil::{human_duration, now_ms, now_rfc3339};

const HELP: &str = "\
probe-wechat：微信 ClawBot 主动推送实测（阶段 0）

用法：probe-wechat [全局选项] <子命令> [子命令选项]

建议的实测顺序：
  1. 先执行 login，用微信扫描二维码并确认。
  2. 再执行 listen：用微信给机器人发一条消息，程序回复“已收到”，
     然后按间隔连续主动推送 12 条，看配额（约 10 条）和限速。
  3. 之后隔 1 小时、12 小时、23 小时、25 小时，各执行一次 push，
     看距离上次收到消息多久后推送仍然成功、何时被拒绝。
  （push 前不要再给机器人发消息，否则会刷新会话凭证。）

子命令：
  login   显示二维码（终端字符画，同时保存 state/qr.png），扫码后保存登录状态
  listen  等待用户消息 -> 保存会话凭证和收到时间 -> 回复“已收到” -> 推送 12 条
          选项：--count N（推送条数）  --interval-secs N（间隔秒数，默认读 config.toml，20）
  push    用保存的会话凭证推送 1 条，并打印距离上次收到消息过了多久
  sample  用保存的会话凭证发 1 条结构化推送格式样例（纯文本，内容虚构）
  burst   快速连发，测限流（会触发几分钟封禁）。必须显式加 --yes 才执行；选项：--count N（默认 30）

全局选项：
  --dir <目录>               probe 目录（默认：编译时的 probes/wechat），读 config.toml，写 state/
  --light-terminal           二维码字符画改为深色填充（浅色背景终端用）
  --mock                     不连腾讯：在本机起模拟 iLink 服务，完整跑 login/listen/push（状态在 state/mock/）
  --mock-elapsed-hours <N>   仅 mock：模拟已过去 N 小时（超过 24 小时，推送返回“失效”）
  -h, --help                 显示本帮助

结果追加到 state/log.jsonl（时间、动作、结果、错误码，不含凭证）。
代理：config.toml 的 proxy（默认 http://127.0.0.1:7892），真实模式下所有请求都走代理。
";

#[derive(Debug, Clone, PartialEq)]
struct Args {
    cmd: String,
    mock: bool,
    elapsed_hours: f64,
    dir: PathBuf,
    interval_secs: Option<u64>,
    count: Option<usize>,
    yes: bool,
    light_terminal: bool,
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Run(Box<Args>),
}

fn parse_args(raw: &[String]) -> Result<Parsed, String> {
    let mut args = Args {
        cmd: String::new(),
        mock: false,
        elapsed_hours: 0.0,
        dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        interval_secs: None,
        count: None,
        yes: false,
        light_terminal: false,
    };
    let mut it = raw.iter();
    fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
        it.next().ok_or_else(|| format!("{flag} 需要一个值"))
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" | "help" => return Ok(Parsed::Help),
            "--mock" => args.mock = true,
            "--yes" => args.yes = true,
            "--light-terminal" => args.light_terminal = true,
            "--mock-elapsed-hours" => {
                args.elapsed_hours = value(&mut it, a)?
                    .parse::<f64>()
                    .map_err(|_| "--mock-elapsed-hours 需要数字".to_string())?;
            }
            "--dir" => args.dir = PathBuf::from(value(&mut it, a)?),
            "--interval-secs" => {
                args.interval_secs = Some(
                    value(&mut it, a)?
                        .parse()
                        .map_err(|_| "--interval-secs 需要整数".to_string())?,
                );
            }
            "--count" => {
                args.count = Some(
                    value(&mut it, a)?
                        .parse()
                        .map_err(|_| "--count 需要整数".to_string())?,
                );
            }
            s if s.starts_with('-') => return Err(format!("未知选项：{s}")),
            s if args.cmd.is_empty() => args.cmd = s.to_string(),
            s => return Err(format!("多余的参数：{s}")),
        }
    }
    if args.cmd.is_empty() {
        return Ok(Parsed::Help);
    }
    if !["login", "listen", "push", "burst", "sample"].contains(&args.cmd.as_str()) {
        return Err(format!("未知子命令：{}", args.cmd));
    }
    if args.elapsed_hours != 0.0 && !args.mock {
        return Err("--mock-elapsed-hours 只能和 --mock 一起用".into());
    }
    Ok(Parsed::Run(Box::new(args)))
}

/// 运行环境：路径、配置、日志、HTTP 客户端，mock 模式下还有模拟服务。
struct Env {
    args: Args,
    paths: Paths,
    cfg: Config,
    log: Logger,
    ilink: ilink::Client,
    /// 二维码登录主机：mock 下是本机模拟服务，真实模式读 config.toml。
    qr_base: String,
    mock_server: Option<MockServer>,
}

impl Env {
    fn tag(&self) -> &'static str {
        if self.args.mock {
            "[MOCK] "
        } else {
            ""
        }
    }

    /// 当前时间（毫秒）。mock 下加上模拟的流逝时间。
    fn sim_now_ms(&self) -> i64 {
        now_ms() + (self.args.elapsed_hours * 3_600_000.0) as i64
    }

    fn mock_base(&self) -> Option<String> {
        self.mock_server.as_ref().map(|s| s.uri())
    }
}

/// 构造 HTTP 客户端。
/// 真实模式：强制走配置里的代理。
/// 唯一例外：mock 模式只访问本机模拟服务，不走代理（`no_proxy`），不会连到外网。
fn build_http(mock: bool, proxy: &str) -> Res<reqwest::Client> {
    let builder = reqwest::Client::builder().use_rustls_tls();
    let client = if mock {
        builder.no_proxy().build()?
    } else {
        builder.proxy(reqwest::Proxy::all(proxy)?).build()?
    };
    Ok(client)
}

async fn setup(args: Args) -> Res<Env> {
    let paths = Paths::new(args.dir.clone(), args.mock);
    let cfg = Config::load(&paths.config())?;
    let http = build_http(args.mock, &cfg.proxy)?;
    let mock_server = if args.mock {
        Some(
            mock::start(mock::MockOptions {
                elapsed_hours: args.elapsed_hours,
                quota_file: paths.mock_quota(),
            })
            .await,
        )
    } else {
        None
    };
    let log = Logger::new(paths.log());
    // mock 与真实走同一条代码路径，只是登录主机指向本机模拟服务
    let qr_base = mock_server
        .as_ref()
        .map_or_else(|| cfg.qr_base_url.clone(), |s| s.uri());
    let ilink = ilink::Client::new(http, ilink::Config::new(qr_base.clone()));
    Ok(Env {
        args,
        paths,
        cfg,
        log,
        ilink,
        qr_base,
        mock_server,
    })
}

fn load_creds(env: &mut Env) -> Res<Credentials> {
    let mut creds = store::load_credentials(&env.paths)?.ok_or("没有登录状态：请先执行 login")?;
    if let Some(base) = env.mock_base() {
        // 每次运行的 mock 服务端口不同，凭证里的地址换成当前这次的
        creds.base_url = base;
    }
    env.log.add_secret(&creds.token);
    Ok(creds)
}

fn load_context(env: &mut Env) -> Res<ContextRecord> {
    let rec = store::load_context(&env.paths)?
        .ok_or("没有会话凭证：请先执行 listen，并用微信给机器人发一条消息")?;
    env.log.add_secret(&rec.context.context_token);
    Ok(rec)
}

fn show_qr(env: &Env, content: &str) -> Res<()> {
    let matrix = qr::encode(content)?;
    println!("{}", qr::render_ascii(&matrix, env.args.light_terminal));
    std::fs::create_dir_all(&env.paths.state)?;
    std::fs::write(env.paths.qr_png(), qr::render_png(&matrix, 8))?;
    println!("二维码已保存为 PNG：{}", env.paths.qr_png().display());
    println!("二维码内容（字符画扫不出来时可转成二维码）：{content}");
    Ok(())
}

fn finish_login(env: &mut Env, creds: &Credentials) -> Res<()> {
    store::save_credentials(&env.paths, creds)?;
    env.log.add_secret(&creds.token);
    env.log.record(
        "login",
        true,
        None,
        json!({"mock": env.args.mock, "saved_to": env.paths.credentials().display().to_string()}),
    )?;
    println!(
        "{}登录成功，登录状态已保存到 {}（内容是密钥，不要外传）",
        env.tag(),
        env.paths.credentials().display()
    );
    Ok(())
}

async fn cmd_login(env: &mut Env) -> Res<i32> {
    match login_flow(env).await {
        Ok(code) => Ok(code),
        Err(e) => {
            let info = describe(&e);
            env.log.record(
                "login",
                false,
                info.errcode,
                json!({"kind": info.kind, "message": env.log.redact(&info.message)}),
            )?;
            println!("登录失败：{}", env.log.redact(&info.message));
            Ok(1)
        }
    }
}

/// 登录：取二维码，然后轮询扫码状态。mock 与真实模式走同一段代码，只是主机不同。
async fn login_flow(env: &mut Env) -> Result<i32, ilink::Error> {
    const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
    let qr = env.ilink.get_qr_code().await?;
    if let Err(e) = show_qr(env, &qr.qrcode_img_content) {
        println!("显示二维码失败：{e}");
        return Ok(1);
    }
    if env.args.mock {
        println!("[MOCK] 模拟扫码：等待、已扫码、已确认……");
    } else {
        println!("请用微信扫描二维码，并在手机上确认……");
    }
    let started = Instant::now();
    let mut poll_base = env.qr_base.clone();
    let mut verify_code: Option<String> = None;
    let mut last = String::new();
    let pause = Duration::from_millis(if env.args.mock { 300 } else { 1000 });
    while started.elapsed() < LOGIN_TIMEOUT {
        let status = env
            .ilink
            .poll_qr_status(&poll_base, &qr.qrcode, verify_code.as_deref())
            .await?;
        let name = match &status {
            QrStatus::Wait => "wait",
            QrStatus::Scaned => "scaned",
            QrStatus::Confirmed(_) => "confirmed",
            QrStatus::Expired => "expired",
            QrStatus::NeedVerifyCode => "need_verifycode",
            QrStatus::VerifyCodeBlocked => "verify_code_blocked",
            QrStatus::BindedRedirect => "binded_redirect",
            QrStatus::ScanedButRedirect { .. } => "scaned_but_redirect",
            QrStatus::Unknown(s) => s.as_str(),
        };
        if name != last {
            println!("状态：{name}");
            last = name.to_string();
        }
        match status {
            QrStatus::Confirmed(creds) => {
                if let Err(e) = finish_login(env, &creds) {
                    println!("保存登录状态失败：{e}");
                    return Ok(1);
                }
                return Ok(0);
            }
            QrStatus::Expired => {
                println!("二维码已过期，请重新执行 login");
                return Ok(1);
            }
            QrStatus::VerifyCodeBlocked | QrStatus::BindedRedirect => {
                println!("登录被服务端拒绝（{name}）");
                return Ok(1);
            }
            QrStatus::ScanedButRedirect { redirect_host } => {
                if !redirect_host.is_empty() {
                    poll_base = format!("https://{redirect_host}");
                }
            }
            QrStatus::NeedVerifyCode => {
                println!("请输入手机上显示的验证码：");
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).is_err() {
                    return Ok(1);
                }
                verify_code = Some(line.trim().to_string());
            }
            QrStatus::Wait | QrStatus::Scaned | QrStatus::Unknown(_) => {}
        }
        tokio::time::sleep(pause).await;
    }
    println!("登录超时");
    Ok(1)
}

/// 发一条文本，打印并记录结果。返回 (是否成功, errcode)。
#[allow(clippy::too_many_arguments)]
async fn send_one(
    env: &Env,
    creds: &Credentials,
    ctx: &Context,
    action: &str,
    seq: usize,
    total: usize,
    since_received_secs: i64,
    text: &str,
) -> (bool, Option<i32>) {
    let started = Instant::now();
    let sent_at = now_rfc3339();
    let result = env.ilink.send_text(creds, ctx, text).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let (ok, errcode, detail) = match &result {
        Ok(_) => (
            true,
            None,
            json!({"seq": seq, "total": total, "sent_at": sent_at, "latency_ms": latency_ms,
                   "since_received_secs": since_received_secs}),
        ),
        Err(e) => {
            let info = describe(e);
            (
                false,
                info.errcode,
                json!({"seq": seq, "total": total, "sent_at": sent_at, "latency_ms": latency_ms,
                       "since_received_secs": since_received_secs,
                       "kind": info.kind, "http_status": info.http_status,
                       "message": env.log.redact(&info.message)}),
            )
        }
    };
    if let Err(e) = env.log.record(action, ok, errcode, detail) {
        eprintln!("写日志失败：{e}");
    }
    match &result {
        Ok(_) => println!(
            "{}{} 成功  {sent_at}  ({latency_ms}ms)",
            env.tag(),
            seq_label(seq, total)
        ),
        Err(e) => {
            let info = describe(e);
            println!(
                "{}{} 失败  {sent_at}  类别={} errcode={} 信息={}",
                env.tag(),
                seq_label(seq, total),
                info.kind,
                info.errcode.map_or("-".to_string(), |c| c.to_string()),
                env.log.redact(&info.message)
            );
        }
    }
    (ok, errcode)
}

fn seq_label(seq: usize, total: usize) -> String {
    if seq == 0 {
        "[回复“已收到”]".to_string()
    } else {
        format!("[{seq}/{total}]")
    }
}

fn push_text(seq: usize, total: usize) -> String {
    format!("[推送测试 {seq}/{total}] 发送时间 {}", now_rfc3339())
}

fn print_summary(env: &Env, results: &[(bool, Option<i32>)]) {
    let ok = results.iter().filter(|r| r.0).count();
    let first_fail = results.iter().position(|r| !r.0).map(|i| i + 1);
    println!(
        "{}汇总：共 {} 条，成功 {} 条，失败 {} 条；首个失败序号：{}",
        env.tag(),
        results.len(),
        ok,
        results.len() - ok,
        first_fail.map_or("无".to_string(), |n| n.to_string())
    );
}

async fn cmd_listen(env: &mut Env) -> Res<i32> {
    let creds = load_creds(env)?;
    let count = env.args.count.unwrap_or(env.cfg.push_count);
    let interval =
        Duration::from_secs(env.args.interval_secs.unwrap_or(env.cfg.push_interval_secs));
    let mut cursor = store::load_cursor(&env.paths).unwrap_or_default();
    println!(
        "{}等待用户消息：请用微信给机器人发一条消息（Ctrl-C 退出）……",
        env.tag()
    );
    loop {
        let polled = tokio::select! {
            r = env.ilink.get_updates(&creds, &cursor) => r,
            _ = tokio::signal::ctrl_c() => {
                println!("已中断");
                return Ok(130);
            }
        };
        let updates = match polled {
            Ok(u) => u,
            Err(e) => {
                let info = describe(&e);
                env.log.record(
                    "poll",
                    false,
                    info.errcode,
                    json!({"kind": info.kind, "message": env.log.redact(&info.message)}),
                )?;
                if info.kind == "session_expired" {
                    println!("登录状态已失效（-14），请重新执行 login");
                } else {
                    println!("收消息出错：{}", env.log.redact(&info.message));
                }
                return Ok(1);
            }
        };
        if updates.cursor_advanced {
            cursor = updates.cursor.clone();
            store::save_cursor(&env.paths, &cursor)?;
        }
        for msg in &updates.messages {
            if !msg.is_from_user() {
                continue;
            }
            let received_at_ms = now_ms();
            let Some(context) = msg.context(&creds.account_id, received_at_ms) else {
                println!("收到消息但没有会话凭证，忽略");
                continue;
            };
            env.log.add_secret(&context.context_token);
            println!(
                "{}收到用户消息（{}），保存会话凭证",
                env.tag(),
                timeutil::format_ms(received_at_ms)
            );
            store::save_context(
                &env.paths,
                &ContextRecord {
                    context: context.clone(),
                    received_at_ms,
                },
            )?;
            env.log.record(
                "receive",
                true,
                None,
                json!({"received_at": timeutil::format_ms(received_at_ms),
                       "text_chars": msg.text().chars().count()}),
            )?;
            let (reply_ok, _) =
                send_one(env, &creds, &context, "reply", 0, 0, 0, mock::ACK_TEXT).await;
            if !reply_ok {
                println!("回复失败，仍继续推送测试");
            }
            let mut results = Vec::new();
            for seq in 1..=count {
                if seq > 1 {
                    tokio::select! {
                        _ = tokio::time::sleep(interval) => {}
                        _ = tokio::signal::ctrl_c() => {
                            println!("已中断");
                            print_summary(env, &results);
                            return Ok(130);
                        }
                    }
                }
                let since = (env.sim_now_ms() - received_at_ms) / 1000;
                results.push(
                    send_one(
                        env,
                        &creds,
                        &context,
                        "push",
                        seq,
                        count,
                        since,
                        &push_text(seq, count),
                    )
                    .await,
                );
            }
            print_summary(env, &results);
            println!("日志：{}", env.paths.log().display());
            return Ok(0);
        }
    }
}

async fn cmd_push(env: &mut Env) -> Res<i32> {
    let creds = load_creds(env)?;
    let rec = load_context(env)?;
    let since = (env.sim_now_ms() - rec.received_at_ms) / 1000;
    println!(
        "{}上次收到用户消息：{}；距今 {}（{} 秒）",
        env.tag(),
        timeutil::format_ms(rec.received_at_ms),
        human_duration(since),
        since
    );
    let (ok, _) = send_one(
        env,
        &creds,
        &rec.context,
        "push_single",
        1,
        1,
        since,
        &push_text(1, 1),
    )
    .await;
    Ok(if ok { 0 } else { 1 })
}

/// 结构化推送的纯文本格式样例（内容全是虚构，不用 markdown 符号）。
fn sample_text() -> String {
    [
        "【📰 消息面速报 · 格式样例】",
        "━━━━━━━━",
        "🔴 重要（2）",
        "1. 某交易所宣布上线虚构代币 AAA",
        "    影响：短期利好 AAA，可能带动同板块情绪",
        "🔗 https://example.com",
        "2. 某稳定币发行方公布虚构储备报告",
        "    影响：市场对稳定币兑付的担忧缓和",
        "🔗 https://example.com",
        "━━━━━━━━",
        "🟡 关注（3）",
        "1. 某公链计划下月升级主网（虚构）",
        "🔗 https://example.com",
        "2. 某机构提交虚构现货产品申请",
        "🔗 https://example.com",
        "3. 某项目方公布虚构代币解锁计划",
        "🔗 https://example.com",
        "━━━━━━━━",
        "🟢 其他（4）",
        "另有 4 条低影响消息，已归档不推送。",
        "━━━━━━━━",
        "共 9 条 · 重要 2 · 关注 3 · 其他 4 · 本消息为格式样例，内容均为虚构",
    ]
    .join("\n")
}

async fn cmd_sample(env: &mut Env) -> Res<i32> {
    let creds = load_creds(env)?;
    let rec = load_context(env)?;
    let since = (env.sim_now_ms() - rec.received_at_ms) / 1000;
    let text = sample_text();
    println!("发送格式样例：\n{text}");
    let (ok, errcode) = send_one(env, &creds, &rec.context, "sample", 1, 1, since, &text).await;
    if !ok {
        match errcode {
            Some(-2) => println!("返回 -2：额度用完，不重试"),
            Some(-14) => println!("返回 -14：会话过期，不重试"),
            _ => {}
        }
    }
    Ok(if ok { 0 } else { 1 })
}

async fn cmd_burst(env: &mut Env) -> Res<i32> {
    if !env.args.yes {
        println!("burst 会快速连发并可能触发几分钟的封禁，必须显式加 --yes 才会执行。");
        return Ok(2);
    }
    let creds = load_creds(env)?;
    let rec = load_context(env)?;
    let count = env.args.count.unwrap_or(30);
    let mut results = Vec::new();
    for seq in 1..=count {
        let since = (env.sim_now_ms() - rec.received_at_ms) / 1000;
        results.push(
            send_one(
                env,
                &creds,
                &rec.context,
                "burst",
                seq,
                count,
                since,
                &push_text(seq, count),
            )
            .await,
        );
    }
    print_summary(env, &results);
    Ok(0)
}

async fn run(args: Args) -> Res<i32> {
    let mut env = setup(args).await?;
    if env.args.mock {
        println!(
            "【MOCK 模式】请求发往本机模拟服务（{}），不走代理、不连腾讯；状态目录：{}",
            env.mock_base().unwrap_or_default(),
            env.paths.state.display()
        );
    } else {
        println!("所有请求走代理：{}", env.cfg.proxy);
    }
    match env.args.cmd.as_str() {
        "login" => cmd_login(&mut env).await,
        "listen" => cmd_listen(&mut env).await,
        "push" => cmd_push(&mut env).await,
        "burst" => cmd_burst(&mut env).await,
        "sample" => cmd_sample(&mut env).await,
        other => Err(format!("未知子命令：{other}").into()),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&raw) {
        Ok(Parsed::Help) => {
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(a)) => *a,
        Err(e) => {
            eprintln!("参数错误：{e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("错误：{e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_global_and_sub_options() {
        let Parsed::Run(a) = parse_args(&s(&[
            "--mock",
            "--mock-elapsed-hours",
            "25",
            "listen",
            "--count",
            "3",
            "--interval-secs",
            "1",
        ]))
        .unwrap() else {
            panic!("expected run")
        };
        assert!(a.mock);
        assert_eq!(a.elapsed_hours, 25.0);
        assert_eq!(
            (a.cmd.as_str(), a.count, a.interval_secs),
            ("listen", Some(3), Some(1))
        );
    }

    #[test]
    fn sample_text_is_plain() {
        let t = sample_text();
        assert!(t.starts_with("【"));
        assert!(t.contains("━━━━━━━━"));
        assert_eq!(t.matches("🔗 ").count(), 5);
        for bad in ["#", "**", "> "] {
            assert!(!t.contains(bad), "contains {bad}");
        }
        assert!(parse_args(&s(&["sample"])).is_ok());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_args(&s(&["nope"])).is_err());
        assert!(parse_args(&s(&["push", "--mock-elapsed-hours", "5"])).is_err());
        assert!(parse_args(&s(&["--bogus", "push"])).is_err());
        assert_eq!(parse_args(&s(&[])).unwrap(), Parsed::Help);
    }

    #[test]
    fn help_mentions_workflow() {
        for needle in [
            "login",
            "listen",
            "push",
            "1 小时",
            "12 小时",
            "23 小时",
            "25 小时",
            "--yes",
        ] {
            assert!(HELP.contains(needle), "help missing {needle}");
        }
    }

    #[test]
    fn real_mode_requires_proxy_mock_does_not() {
        assert!(build_http(false, "not a url").is_err());
        assert!(build_http(false, "http://127.0.0.1:7892").is_ok());
        assert!(build_http(true, "").is_ok());
    }
}
