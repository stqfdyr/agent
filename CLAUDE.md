# monitor-agent

[monitor](https://github.com/stqfdyr/monitor) 探针的 Linux agent。hub 在另一个仓库。

## 两条铁律

**用名字引用，别用编号**——hub 仓库也有一组铁律，编号对不上：那边的第 2 条就是这里的口径铁律，
那边的第 1 条（总流量永不回退）在这里根本不存在，因为累加压根不在 agent 这边做。

1. **口径铁律 —— 数字必须和 `free` / `df` 对得上。** 这是这个 agent 存在的理由之一：内存和硬盘的
   现成写法（`sysinfo` 那对 API、htop 的公式）口径都不对，而且错得看不出来。改采集代码前读
   [docs/data-accuracy.md](docs/data-accuracy.md)
2. **无状态铁律 —— 不写文件、不记忆跨重启的状态。** 只上报「此刻内核说的数字」，累加是 hub 的事。
   理由（agent 跑在别人的机器上，可能被重装、迁移、kill -9，持久状态等于给每台机器发一个会坏的
   文件）见 hub 仓库 [decisions.md](https://github.com/stqfdyr/monitor/blob/main/docs/decisions.md)
   的「agent 完全无状态」。加持久化之前先问用户

## 明确不做的

不要「顺手」加回来，想加先问用户：

- **自动更新**（升级方式是重跑安装命令，一行的事；自更新意味着 agent 能以 root 下载执行任意二进制）
- **跨平台**（Windows / macOS / BSD）——只支持 Linux 是刻意的，直接读 `/proc` 才能修好那几个口径问题。加跨平台等于推翻整个 `collect.rs`
- ICMP / HTTP ping（只保留 TCP）
- 远程命令执行、terminal
- **日志框架**（`tracing` / `log` / `env_logger`）——全仓库两条日志，都是事后翻的运行记录，
  `eprintln!` 就够。`tracing` + `tracing-subscriber` 曾占二进制的 20%（431 KiB）和 14 个依赖，
  换来的是在 journald 自己的时间戳旁边再印一个，还是不同时区的

## 工作方式

- 用 `ponytail` skill（full），别过度设计、别过度测试。一段非平凡逻辑留一个能跑的检查就够
- 注释写约束，不写调试过程；解释比代码长就删解释
- 断言要能被证伪：写完想一下**什么改动会让它变红**，想不出来就别写。口径铁律的比对必须打真机，
  构造数据证明不了读的是对的字段。验证手法见 hub 仓库
  [development.md](https://github.com/stqfdyr/monitor/blob/main/docs/development.md) 的「变异检查」
- **期望值不要照着实现的公式写，更不要在测试里重抄一遍公式。** 那种断言改公式就红，却不知道公式
  对不对——swap 减 SwapCached 和 CPU 报成 idle% 各在这上面躺过一次，全套测试一路绿。数字要么对
  外部权威（`free` / `df`，进 `crosscheck`），要么至少让断言打在被测函数上
- `Metrics` / `Facts` struct 就是线上协议的权威定义，别再写一份字段列表去同步
- 改了上报字段就是改了协议，hub 那边要同步

## 常用命令

```bash
cargo test                # 17 个
cargo clippy --all-targets
cargo test crosscheck -- --nocapture   # 采集值与 free / df 自动比对，顺带打印
```
