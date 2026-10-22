#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = app::run(&args).await;
    std::process::exit(code);
}
