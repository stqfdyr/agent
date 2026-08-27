# monitor-agent

[monitor](https://github.com/stqfdyr/monitor) 探针的 Linux agent。采集本机指标，通过 WebSocket 上报给 hub。

一个静态链接的单文件，无运行时依赖，常驻内存几 MB。

## 特点

- **只支持 Linux**，直接读 `/proc` 和 `statvfs`，不用 sysinfo
- **数字和系统工具对得上**：内存等于 `free` 的 used 列，硬盘等于 `df` 的 Used 列。见 [docs/data-accuracy.md](docs/data-accuracy.md)
- **完全无状态**：不写任何文件、不记忆跨重启的状态。流量累加是 hub 的事
- **token 走 `Authorization` 头**，不进反代的 access log
- **拒绝明文传输**：非回环地址不允许 `ws://`

## 安装

正常情况下不用手动装——在 hub 的面板里添加节点，复制出来的命令粘到目标 VPS 上即可：

```bash
curl -fsSL https://your-hub/install.sh | sh -s -- --server https://your-hub --token xxx
```

装完是一个 systemd 服务，token 存在 `/etc/monitor/agent.env`（0600）。

手动运行：

```bash
monitor-agent --server https://your-hub --token xxx [--interval 1]
```

也可以用 `MONITOR_SERVER` / `MONITOR_TOKEN` 环境变量代替参数。

| 参数 | 默认 | 说明 |
|---|---|---|
| `--server` | 必填 | hub 的地址 |
| `--token` | 必填 | 面板里生成的节点 token |
| `--interval` | 1 | 上报间隔（秒），1–3600。hub 生成的安装命令也默认传 1 |

## 上报什么

`src/collect.rs` 里的 `Facts` 和 `Metrics` 两个 struct 就是权威定义——它们直接 `Serialize` 成线上的 JSON，不存在第二份需要同步的字段列表。

大致是：CPU、负载、内存、swap、硬盘、网卡收发速率与累计计数器、TCP/UDP 连接数、进程数、运行时间，外加连接时上报一次的静态信息（主机名、系统、内核、架构、虚拟化类型、CPU 型号）。

其中两个字段值得单独说：

- **`net_rx_total` / `net_tx_total`** 是内核的 lifetime 计数器，**原样上报**，不加工。累加由 hub 负责
- **`boot_id`** 来自 `/proc/sys/kernel/random/boot_id`，是 hub 判断"这台机器重启过、计数器归零了"的唯一依据。**不要删**

完整的协议说明在 hub 仓库的 [docs/architecture.md](https://github.com/stqfdyr/monitor/blob/main/docs/architecture.md)。

## 构建

需要 Rust stable。

```bash
cargo build --release
cargo test          # 12 个
cargo clippy --all-targets
```

发布用 musl 静态链接，推 `v*` tag 触发 `.github/workflows/release.yml`。

## 许可

MIT
