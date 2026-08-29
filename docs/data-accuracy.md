# 数据口径

内存和硬盘是这个 agent 最容易报错、报错了又最不容易被发现的两个数——它们永远会给出一个看着挺
合理的数字。改 `src/collect.rs` 之前请读完这篇。

**验收标准很硬：面板上的数字必须和目标机器上 `free` / `df` 的输出对得上。**

## 内存

### 现成的写法错在哪

最省事的做法是 `sysinfo` 的 `used_memory()`，而它把 **page cache 算成已用内存**。Linux 会拿所有
空闲内存做磁盘缓存，所以一台开机跑了几天的机器，cache 通常有好几个 GB——面板上就显示成内存快满了，
实际上完全没有压力。

### 现在怎么算

```rust
used = MemTotal - MemAvailable
```

`MemAvailable` 是内核自己给出的估计值：**在不触发 swap 的前提下还能分配多少**。它已经扣掉了可回收的 cache 和 slab。

这也正好是现代 `free(1)` 的 used 列的算法（procps 的 `free.c`：`mem_used = kb_main_total - kb_main_available`）。

`MemAvailable` 不存在时（3.14 以前的内核、某些容器）退回 `MemFree + Buffers + Cached`。

### 为什么不用 htop 的公式

另一条广泛使用的算法是 htop 的：

```
used = MemTotal - (MemFree + Buffers + Cached + SReclaimable) + Shmem
```

两条公式在同一台机器上给出的数字不一样。在 cc 上同一时刻取值：

```
MemTotal 468400  MemFree 38124  MemAvailable 353608
Buffers 57820  Cached 217888  SReclaimable 54440  Shmem 2620   (kB)

本项目   total - MemAvailable                              = 112.1 MiB
htop     total - (free+cached+sreclaimable+buffers) + shmem = 100.4 MiB
差        11.7 MiB
```

差额拆开来是两笔：内核认为**收不回来**的那部分 cache（14.3 MiB），减去 htop 额外计入的 `Shmem`
（2.6 MiB，共享内存确实占着物理页，这一笔 htop 算得对）。

**两个公式回答的不是同一个问题。** htop 那条假设 cache、buffer、可回收 slab 全都能收回来，答的是
「进程大概占了多少」；内核比谁都清楚其中有一部分（脏页、被 mlock 的、正在映射的）收不回来，
`MemAvailable` 已经把它们扣掉了，答的是「还剩多少能用」。

面板上那根内存条要回答的是后者，所以用 `MemAvailable`：

- 它是内核的判断，不是用户态的假设；htop 公式会**系统性低估**内存压力，而且越是内存吃紧、cache 越少，
  两者差得越多——恰恰在最需要准的时候最不准
- 用户对照的是 `free -h`，不是 htop
- 一行减法 vs 五个字段的公式，前者更难写错

内存是唯一有这种分歧的指标。硬盘、进程数、UDP、uptime 都只有一个读法，读的就是内核给的那个数。
TCP 连接数看着会跳，那是 TIME_WAIT 每秒都在变，不是口径问题——同一时刻 sockstat
（`inuse 8 + tw 8 + TCP6 inuse 3 = 19`）和 `/proc/net/tcp` 的行数（19）是对得上的。

原本第一版实现的就是 htop 公式，实机对照后换掉了。commit `8fa5ab8` 之后的修改。

### swap

```rust
used = SwapTotal - SwapFree - SwapCached
```

`SwapCached` 是已经换回内存但 swap 里还留着副本的页，不算真正占用。

## 硬盘

### 现成的写法错在哪

最直觉的写法是 `total_space - available_space`（`sysinfo` 给的也是这一对）。

ext4 默认给 root 预留 5% 的块（`tune2fs -m`）。这部分块普通用户用不了，所以不计入 `available`，但它们**也没被使用**。用 `total - available` 就等于把这 5% 记成已用——一块刚格式化的 100 GB 盘会显示已用 5 GB。

### 现在怎么算

```rust
total = f_blocks * f_frsize
used  = (f_blocks - f_bfree) * f_frsize    // f_bfree 是原始空闲块，含预留
```

这和 `df` 的 Used 列**完全一致**（不是近似，是逐字节相同）。

区别就在 `f_bfree`（所有空闲块）和 `f_bavail`（非特权用户可用的块）——差值就是 root 预留量。

### 挂载点去重

`parse_mounts()` 读 `/proc/self/mounts`，然后：

- 按 fstype 排除伪文件系统（tmpfs、proc、sysfs、overlay、squashfs……完整名单在 `SKIP_FSTYPES`）
- 排除设备名不以 `/` 开头的（zfs 和 btrfs 例外，它们的"设备"是池名/子卷）
- **按源设备去重**——同一块盘挂两次（bind mount、btrfs 子卷）只算一次
- zfs 按池名去重（`tank/set1` 和 `tank/set2` 共享 `tank` 的空间）

不去重的话，一台有 bind mount 的机器硬盘容量会翻倍。

测试：`mounts_drop_pseudo_filesystems_and_duplicate_devices`。它的样本里刻意留了两行只被其中
一道检查挡住的挂载（`/dev/loop0 … squashfs` 只有 fstype 名单拦得住，`none … ext4` 只有设备名
检查拦得住）——不然两道检查互为备份，删掉任何一道测试都照样绿。

挂载表**每次采样都重读**，不在启动时缓存：agent 起来之后加挂的数据盘，否则要等到重启 agent
才会出现在容量里。`/proc/self/mounts` 只有几 KB，比后面那次 `statvfs` 便宜。

## CPU

读 `/proc/stat` 第一行的 jiffies，算**两次采样之间的差值**：

```
busy% = (总增量 - idle 增量) / 总增量 * 100
```

`idle` 取 `idle + iowait` 两项之和（都是 CPU 没在干活的时间）。

总量只累加**前 8 个字段**（user 到 steal）。最后两个 `guest` / `guest_nice` 是内核已经计进
user 和 nice 的时间，再加一遍就是重复计算，会把跑虚拟化的宿主机的 CPU 占用算低。

**第一次采集返回 0**，因为没有基线。返回自开机以来的平均值会是个毫无意义的数字。

## 连接数

`tcp` / `udp` 来自 `/proc/net/sockstat` 和 `sockstat6`，各取一个 `inuse`，TCP 再加上 v4 那行的
`tw`（TIME_WAIT 的套接字两个协议族共用这一个计数器）。

原来的做法是数 `/proc/net/tcp{,6}` 的行数：结果一样，但要内核把整张连接表格式化成文本、agent
再逐行数一遍——一台几千连接的机器上就是每秒读几百 KB，只为得到一个内核本来就在数的整数。

测试：`socket_counts_come_from_sockstat_not_the_connection_table`，用的是真机上两种算法都得出
86 的那组数据。

## 网络速率

`net_rx` / `net_tx` 是 agent 自己算的瞬时速率（B/s），用两次采样的计数器差值除以经过的时间。

计数器回退时返回 `0` 而不是负数或巨大的正数——不要在重启的瞬间画出一根冲天的尖峰。

`net_rx_total` / `net_tx_total` 是**原样上报**的内核计数器，累加是 hub 的事，见 hub 仓库的 [traffic.md](https://github.com/stqfdyr/monitor/blob/main/docs/traffic.md)。

网卡过滤：`SKIP_IFACES` 排除 lo、docker、veth、br-、virbr、tap、tun、cni 等前缀，写死在 `src/collect.rs`。

## 网络延迟

`tcp_ping()` 量的是一次 TCP 握手的往返，单位毫秒；测不到就是 `-1`。域名在计时开始**之前**解析，
所以 DNS 不进读数——把域名直接交给 `TcpStream::connect` 是解析和连接一起做的，域名目标上解析
往往就是读数的大头。

### 超时为什么是 900 毫秒

**因为 Linux 的首个 SYN 重传定时器是 1 秒。** 等得比它久，丢掉的 SYN 就不会失败——它会「迟到地
成功」，而 `connect` 交回来的是重传定时器加上往返，不是往返。那不是慢，是另一种测量披着毫秒的皮。

生产库里 3721 个样本，异常值的分布把这件事说得很白：

```
   0– 500 ms : 3559 个   ← 正常往返，中位数 195
 500– 900 ms :    0 个   ← 空的
1100–1299 ms :  104 个   ← 1000 ms 首次重传 + 200 ms 往返
3200–3299 ms :   57 个   ← 1000+2000 ms 两次重传 + 往返
```

中间那段空白就是证据：连续的网络抖动不会留下空隙，离散的内核定时器才会。

把超时压到重传之前，回来的读数就只可能属于「第一个 SYN 就成功」的握手，也就必然是真往返；丢了的
那次变成 `-1`，而它本来就是丢包。代价是真往返超过 900 毫秒的链路会被报成不可达——到那个份上它对任何
用途来说也确实是。

受控实验（环回口注入 50% SYN 丢包，真实往返 0 ms）：

| 超时 | 成功 | 成功读数中位 | ≥900 ms 的假读数 |
|---|---|---|---|
| 5 秒（旧） | 26/30 | **1010 ms** | 14 个 |
| 0.9 秒（新） | 10/30 | **0 ms** | 0 个 |

旧超时在一条零延迟的链路上报出 1010 毫秒，新超时报出真值和丢包。

### 否决过的做法：超时之后重测

另一条路是把超时留得很宽，读数超过 1000 ms 时重测几次，若重测明显变快（差值 > 800 ms）就判定
中间发生过重传，把这次记为失败。结论会和现在一致——重传期间的数字不能当延迟用——但代价是每个可疑
样本多花最多 3 次握手，而且重测测的是「后来那一刻」，已经不是原本要问的那一刻。

截断超时拿到同样的结果，不额外发一个包，逻辑就是一个常量。

## 怎么验证

`collect.rs` 里的 `crosscheck` 测试模块自己去跑 `free` 和 `df` 并比对，不用人工核对：

```bash
cargo test crosscheck -- --nocapture     # 仍然打印，同时会断言
```

容差 64 MiB，留给两次取值之间机器自己的变化。它比任何一种错口径都小一个数量级——本机上
`df` 排除的 root 预留是 3.2 GiB，sysinfo 会记成已用的 page cache 是 2.4 GiB，htop 公式的偏差
288 MiB，全都远在容差之外。差到几百 MB 就是口径错了，不是抖动。

**这个比对必须打真机**：构造一段 `/proc/meminfo` 文本只能证明算术没写错，证明不了读的是对的
字段——而读错字段正是这个 agent 要修的原始 bug。用 `f_bavail` 代替 `f_bfree` 在构造数据上
一样能算出自洽的结果，只有跟 `df` 一比才露馅。

系统上没有 `free` 或 `df` 时测试**失败**，不是跳过：一个可以静默跳过比对的测试等于没有。

最近一次实机对照（Debian 12，3.8 GiB 内存，59 GiB 盘）：

| | monitor | 系统工具 |
|---|---|---|
| 内存 | 1.01 GiB / 3.82 GiB | `free` 1.02 / 3.82 |
| 硬盘 | 11.83 GiB / 58.94 GiB | `df` 11.83 / 58.94 |

## 不要引入 sysinfo

agent 只跑 Linux（用户的决定，见 hub 仓库的 [decisions.md](https://github.com/stqfdyr/monitor/blob/main/docs/decisions.md)），直接读 `/proc` 更准、更小、更好懂。sysinfo 的内存和硬盘口径就是这篇文档在修的东西——把它加回来等于把 bug 加回来。
