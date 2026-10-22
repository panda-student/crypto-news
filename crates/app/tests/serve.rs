use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn assembles_all_official_plugins() {
    let dir = std::env::temp_dir().join(format!("app-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let text = app::prepare_data_dir(&dir).unwrap();
    // secrets 会访问系统钥匙串，测试里停用
    let text = text.replace("127.0.0.1:8730", "127.0.0.1:0")
        + "
[plugins.secrets]
enabled = false
";
    let cfg = app::parse_config(&text).unwrap();
    let hub = app::build_hub(&cfg, &dir).await.unwrap();
    hub.start().await.unwrap();

    let v = hub
        .call("hub-admin", "plugins", serde_json::json!({}))
        .await
        .unwrap();
    let names: Vec<String> = v["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    for n in [
        "auth",
        "scheduler",
        "collect",
        "rss",
        "okx-ann",
        "binance-ann",
        "web",
    ] {
        assert!(names.iter().any(|x| x == n), "缺少插件 {n}：{names:?}");
    }
    tokio::time::timeout(Duration::from_secs(20), hub.shutdown())
        .await
        .expect("shutdown 超时");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn start_stop_and_lock() {
    let dir = std::env::temp_dir().join(format!("app-it-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let text = app::prepare_data_dir(&dir).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    // secrets 会访问系统钥匙串，测试里停用；依赖它的插件会处于等待依赖状态
    let text = text
        .replace("127.0.0.1:8730", &format!("127.0.0.1:{port}"))
        .replace("http://127.0.0.1:7892", "http://127.0.0.1:1")
        + "\n[plugins.secrets]\nenabled = false\n";
    std::fs::write(dir.join("config.toml"), text).unwrap();

    let running = app::start(Some(dir.clone())).await.unwrap();
    assert_eq!(running.listen.port(), port);
    assert_eq!(
        app::web_url(running.listen),
        format!("http://127.0.0.1:{port}/")
    );

    let v = running
        .hub
        .call("hub-admin", "plugins", serde_json::json!({}))
        .await
        .unwrap();
    let names: Vec<String> = v["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    for n in [
        "agent",
        "dispatch",
        "llm",
        "rulebook",
        "screen",
        "maintenance",
    ] {
        assert!(names.iter().any(|x| x == n), "缺少插件 {n}：{names:?}");
    }

    let second = app::start(Some(dir.clone())).await;
    assert!(matches!(second, Err(app::AppError::Locked(_))));

    tokio::time::timeout(Duration::from_secs(20), running.stop())
        .await
        .expect("stop 超时");
    let _ = std::fs::remove_dir_all(&dir);
}
