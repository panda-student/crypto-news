# hello-ext

扩展插件示例：单独的可执行文件加 `plugin.toml`，放进插件目录，重启后加载。

- 服务 `hello` 1.0：`greet {name}` 返回 `{text}`；`stats {}` 返回 `{items_seen}`。
- 订阅 `item.collected`，在内存里计数。

## 安装

```
cargo build --release -p hello-ext
```

把 `target/release/hello-ext.exe` 和本目录的 `plugin.toml` 复制到 `<数据目录>/plugins/hello-ext/`，重启 `app serve`，在页面里确认该插件的权限。

`plugin.toml` 的 `entry` 默认是 `hello-ext.exe`（Windows）；其他系统的可执行文件没有 `.exe` 后缀，需要把 `entry` 改成 `hello-ext`。
