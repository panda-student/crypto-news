fn main() {
    if let Err(e) = plugin_sdk::run_extension(hello_ext::HelloExt::default()) {
        eprintln!("hello-ext 退出：{e}");
        std::process::exit(1);
    }
}
