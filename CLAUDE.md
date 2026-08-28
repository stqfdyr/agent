# monitor-agent

[monitor](https://github.com/stqfdyr/monitor) 探针的 Linux agent。hub 在另一个仓库。

## 两条铁律

1. **数字必须和 `free` / `df` 对得上。** 这是这个 agent 存在的理由之一——参考实现（sysinfo、komari）在内存和硬盘上口径是错的。改采集代码前读 [docs/data-accuracy.md](docs/data-accuracy.md)
2. **agent 保持无状态。** 不写文件、不记忆跨重启的状态。它只上报"此刻内核说的数字"，累加是 hub 的事。往这里加持久化之前先问用户

## 明确不做的

不要"顺手"加回来，想加先问用户：

- **自动更新**（升级方式是重跑安装命令，一行的事；自更新意味着 agent 能以 root 下载执行任意二进制）
- **跨平台**（Windows / macOS / BSD）——只支持 Linux 是刻意的，直接读 `/proc` 才能修好那几个口径问题。加跨平台等于推翻整个 `collect.rs`
- ICMP / HTTP ping（只保留 TCP）
- 远程命令执行、terminal

## 工作方式

- 用 `ponytail` skill（full），别过度设计、别过度测试。一段非平凡逻辑留一个能跑的检查就够
- 断言要能被证伪：写完想一下**什么改动会让它变红**，想不出来就别写。铁律一的比对必须打真机，
  构造数据证明不了读的是对的字段。验证手法见 hub 仓库 [development.md](https://github.com/stqfdyr/monitor/blob/main/docs/development.md) 的「变异检查」
- `Metrics` / `Facts` struct 就是线上协议的权威定义，别再写一份字段列表去同步
- 改了上报字段就是改了协议，hub 那边要同步

## 常用命令

```bash
cargo test                # 14 个
cargo clippy --all-targets
cargo test crosscheck -- --nocapture   # 采集值与 free / df 自动比对，顺带打印
```
