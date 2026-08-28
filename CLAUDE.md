# monitor-agent

[monitor](https://github.com/stqfdyr/monitor) 探针的 Linux agent。hub 在另一个仓库。

## 两条铁律

1. **数字必须和 `free` / `df` 对得上。** 这是这个 agent 存在的理由之一——参考实现（sysinfo、komari）在内存和硬盘上口径是错的。改采集代码前读 [docs/data-accuracy.md](docs/data-accuracy.md)
2. **agent 保持无状态。** 不写文件、不记忆跨重启的状态。它只上报"此刻内核说的数字"，累加是 hub 的事。往这里加持久化之前先问用户

## 明确不做的

不要"顺手"加回来，想加先问用户：

- **自更新**（agent 进程不碰自己的二进制。它跑在 `DynamicUser` + `ProtectSystem=strict` 下面，本来也写不了；要自更新就得把这些全撤掉，等于给一个长期在线、解析外来帧的进程 root 写权限。更新由 hub 的 `install.sh` 装的 root 定时器做，见 [hub 的 decisions.md](https://github.com/stqfdyr/monitor/blob/main/docs/decisions.md)）
- **跨平台**（Windows / macOS / BSD）——只支持 Linux 是刻意的，直接读 `/proc` 才能修好那几个口径问题。加跨平台等于推翻整个 `collect.rs`
- ICMP / HTTP ping（只保留 TCP）
- 远程命令执行、terminal

## 工作方式

- 用 `ponytail` skill（full），别过度设计、别过度测试。一段非平凡逻辑留一个能跑的检查就够
- `Metrics` / `Facts` struct 就是线上协议的权威定义，别再写一份字段列表去同步
- 改了上报字段就是改了协议，hub 那边要同步

## 常用命令

```bash
cargo test                # 12 个
cargo clippy --all-targets
cargo test crosscheck -- --nocapture   # 打印采集值，和 free / df 对照
```
