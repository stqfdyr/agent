# 数据口径

内存和硬盘是这个 agent 最容易报错、报错了又最不容易被发现的两个数——错的读法永远给出一个看着
合理的数字。改 `src/collect.rs` 之前请读完这篇。

**验收标准：面板上的数字必须和目标机器上 `free` / `df` 的输出对得上。**

## 内存

### 现成的写法错在哪

最省事的做法是 `sysinfo` 的 `used_memory()`，而它把 **page cache 算成已用内存**。Linux 拿所有
空闲内存做磁盘缓存，所以一台跑了几天的机器 cache 通常有好几个 GB，面板上就显示成内存快满了。

### 现在怎么算

```rust
used = MemTotal - MemAvailable
```

`MemAvailable` 是内核给出的估计：**在不触发 swap 的前提下还能分配多少**，已经扣掉可回收的 cache
和 slab。这也正是现代 `free(1)` 的 used 列的算法（procps 的 `free.c`：
`mem_used = kb_main_total - kb_main_available`）。

`MemAvailable` 不存在时（3.14 以前的内核、某些容器）退回 `MemFree + Buffers + Cached`。判据是
**字段在不在**，不是值等不等于 0：一台真正吃紧的机器 `MemAvailable` 就是 0，按值判断会误走回退
分支，在最需要报准的那一刻把已用内存报低。

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

差额拆开是两笔：内核认为**收不回来**的那部分 cache（14.3 MiB），减去 htop 额外计入的 `Shmem`
（2.6 MiB，共享内存确实占着物理页，这一笔 htop 算得对）。

**两个公式回答的不是同一个问题。** htop 那条假设 cache、buffer、可回收 slab 全都能收回来，答的是
「进程大概占了多少」；内核清楚其中有一部分（脏页、被 mlock 的、正在映射的）收不回来，
`MemAvailable` 已经把它们扣掉，答的是「还剩多少能用」。

面板上那根内存条要回答的是后者，所以用 `MemAvailable`：

- 它是内核的判断，不是用户态的假设。htop 公式**系统性低估**内存压力，而且越是内存吃紧、cache 越少，
  两者差得越多——恰恰在最需要准的时候最不准
- 对照的基准是 `free -h`，不是 htop
- 一行减法比五个字段的公式更难写错

内存和 swap 是仅有的两个「有分歧的读法」的指标，两次都是同一个教训：一个听起来讲得通的调整，换来
一个系统性偏低的数字。硬盘、进程数、UDP、uptime 都只有一个读法。TCP 连接数看着会跳，那是 TIME_WAIT
每秒都在变，不是口径问题——同一时刻 sockstat（`inuse 8 + tw 8 + TCP6 inuse 3 = 19`）和
`/proc/net/tcp` 的行数（19）对得上。

### swap

```rust
used = SwapTotal - SwapFree
```

和 `free` 的 Swap used 列一样，**不减 `SwapCached`**。

原本减了，理由是「`SwapCached` 是已经换回内存但 swap 里还留着副本的页，不算真正占用」。但那些页
换回内存之后，**swap 设备上的块并没有释放**——内核留着副本正是为了下次换出时不用再写一遍，要到
别的页需要那些块时才回收。所以它们确实占着 swap。

实机对照（ccs，`free -b`）：

| | swap used |
|---|---|
| `free` | 32,968,704 |
| 减 SwapCached | 26,222,592 |
| 差 | 6,746,112（正好一个 `SwapCached`，低 20%） |

**这个错法能活下来，是因为 `crosscheck` 当时只比对内存和硬盘。** 现在它也比对 swap，容差单独设成
1 MiB：swap 变化远比内存慢，而这里要抓的偏差只有几 MB，64 MiB 的内存容差会把它整个放过去。

## 硬盘

### 现成的写法错在哪

最直觉的写法是 `total_space - available_space`（`sysinfo` 给的也是这一对）。

ext4 默认给 root 预留 5% 的块（`tune2fs -m`）。这部分块普通用户用不了，所以不计入 `available`，
但它们**也没被使用**。用 `total - available` 等于把这 5% 记成已用——一块刚格式化的 100 GB 盘会
显示已用 5 GB。

### 现在怎么算

```rust
total = f_blocks * f_frsize
used  = (f_blocks - f_bfree) * f_frsize    // f_bfree 是原始空闲块，含预留
```

这和 `df` 的 Used 列**完全一致**（不是近似，是逐字节相同）。

区别就在 `f_bfree`（所有空闲块）和 `f_bavail`（非特权用户可用的块）——差值就是 root 预留量。

### 挂载点去重

`parse_mounts()` 读 `/proc/self/mounts`，然后：

- 按 fstype 排除伪文件系统和**远端文件系统**（tmpfs、proc、sysfs、overlay、squashfs、
  nfs、cifs、ceph、glusterfs、9p……完整名单在 `SKIP_FSTYPES`）。CIFS 的设备名 `//server/share`
  以 `/` 开头，过得了下面那道设备检查，只有 fstype 名单拦得住——否则一台 50 GB 的 VPS 会因为挂了
  个 NAS 显示 20 TB。NFS 一直没出事纯属巧合：`server:/export` 卡在设备名那道。名单按 `.` 分隔匹配
  变体，所以 `nfs` 不覆盖 `nfs4`，两个都得写；反过来 `fuse` 一条就盖住 `fuse.sshfs`、
  `fuse.mergerfs`、`fuse.lxcfs` 这些「没有本地块设备撑着」的挂载——真有块设备撑着的 fuse（ntfs-3g）
  fstype 是 `fuseblk`，盖不到，仍然计入
- 排除设备名不以 `/` 开头的（zfs 和 btrfs 例外，它们的「设备」是池名/子卷）
- **按源设备去重**——同一块盘挂两次（bind mount、btrfs 子卷）只算一次
- zfs 按池名去重（`tank/set1` 和 `tank/set2` 共享 `tank` 的空间）

不去重的话，一台有 bind mount 的机器硬盘容量会翻倍。

测试：`mounts_drop_pseudo_filesystems_and_duplicate_devices`。样本里刻意留了两行只被其中一道检查
挡住的挂载（`/dev/loop0 … squashfs` 只有 fstype 名单拦得住，`none … ext4` 只有设备名检查拦得住）
——否则两道检查互为备份，删掉任何一道测试都照样绿。

挂载表**每次采样都重读**，不在启动时缓存：否则 agent 起来之后加挂的数据盘要等到重启才出现在容量
里。`/proc/self/mounts` 只有几 KB，比后面那次 `statvfs` 便宜。

## CPU

读 `/proc/stat` 第一行的 jiffies，算**两次采样之间的差值**：

```
busy% = (总增量 - idle 增量) / 总增量 * 100
```

`idle` 取 `idle + iowait` 两项之和（都是 CPU 没在干活的时间）。

总量只累加**前 8 个字段**（user 到 steal）。最后两个 `guest` / `guest_nice` 是内核已经计进
user 和 nice 的时间，再加一遍就是重复计算，会把跑虚拟化的宿主机的 CPU 占用算低。

**第一次采集返回 0**，因为没有基线；返回自开机以来的平均值是个没有意义的数字。

算差值的那段拆成了 `busy_percent(prev, now)` 纯函数，`cpu_percent()` 只负责读 `/proc/stat` 和存
基线。拆之前两件事挤在一个函数里，喂不进构造数据，测试只好重抄一份公式来断言——把 busy 换成 idle
（面板上 CPU 整个反过来）全套测试照样绿。测试：`cpu_percent_needs_a_baseline_then_uses_deltas`。

## 连接数

`tcp` / `udp` 来自 `/proc/net/sockstat` 和 `sockstat6`，各取一个 `inuse`，TCP 再加上 v4 那行的
`tw`（TIME_WAIT 的套接字两个协议族共用这一个计数器）。

原来的做法是数 `/proc/net/tcp{,6}` 的行数：结果一样，但要内核把整张连接表格式化成文本、agent 再
逐行数一遍——一台几千连接的机器上每秒读几百 KB，只为一个内核本来就在数的整数。

测试：`socket_counts_come_from_sockstat_not_the_connection_table`，用的是真机上两种算法都得出
86 的那组数据。

## 网络速率

`net_rx` / `net_tx` 是 agent 自己算的瞬时速率（B/s），用两次采样的计数器差值除以经过的时间。

计数器回退时返回 `0` 而不是负数或巨大的正数——不要在重启的瞬间画出一根冲天的尖峰。

`net_rx_total` / `net_tx_total` 是**原样上报**的内核计数器，累加是 hub 的事，见 hub 仓库的 [traffic.md](https://github.com/stqfdyr/monitor/blob/main/docs/traffic.md)。

### 网卡过滤

口径是**同一份物理线上的字节只数一次**。两层规则，都写死在 `src/collect.rs`：

- `SKIP_IFACES`——既不是本机流量、也不是本机身份的网卡：lo、docker、veth、br-、virbr、cni、
  podman、kube 等前缀，加上 tun / tap / **wg**。隧道口是重复计数：`wg0` 上的 300 字节离开机器时，
  是承载它的那个 UDP 包在 `eth0` 上再记一次。`wg` 漏了很久，而测试把这个双计当成正确结果断言了下来
- `is_stacked()`——叠在别的网卡之上的设备：bond、br、vmbr、vlan 前缀，以及 `eth0.100` 这种带点的
  VLAN 子接口。内核在下层和上层各记一次同一个包，两个都算就是双倍流量，而 hub 的配额是按这对
  生涯计数器算的

**只有流量过滤看 `is_stacked()`，取地址不看。** 「这些字节是不是已经数过」和「这个地址是不是我的」
是两个问题：`vmbr0`（Proxmox）、`bond0` 往往正是持有主机管理地址的那块网卡，拿流量名单去挑地址，
结果是 `Facts.ipv4` 为空、hub 拿不到节点地址。取地址只过 `SKIP_IFACES`。

测试：`net_counts_a_wire_byte_once_however_many_devices_book_it` 和
`a_stacked_device_loses_its_bytes_but_keeps_its_address`，后者把「一个名字被问了两个问题」直接钉住。

## 网络延迟

`tcp_ping()` 量的是一次 TCP 握手的往返，单位毫秒；测不到就是 `-1`。域名在计时开始**之前**解析，
所以 DNS 不进读数——把域名直接交给 `TcpStream::connect` 是解析和连接一起做的，域名目标上解析
往往就是读数的大头。

### 超时为什么是 900 毫秒

**因为 Linux 的首个 SYN 重传定时器是 1 秒。** 等得比它久，丢掉的 SYN 不会失败，而是「迟到地成功」
——`connect` 交回来的是重传定时器加上往返，不是往返，那是另一种测量用着同样的单位。

生产库里 3721 个样本，异常值的分布：

```
   0– 500 ms : 3559 个   ← 正常往返，中位数 195
 500– 900 ms :    0 个   ← 空的
1100–1299 ms :  104 个   ← 1000 ms 首次重传 + 200 ms 往返
3200–3299 ms :   57 个   ← 1000+2000 ms 两次重传 + 往返
```

中间那段空白就是证据：连续的网络抖动不会留下空隙，离散的内核定时器才会。

把超时压到重传之前，回来的读数就只可能属于「第一个 SYN 就成功」的握手，必然是真往返；丢了的那次
变成 `-1`，而它本来就是丢包。代价是真往返超过 900 毫秒的链路会被报成不可达。

受控实验（环回口注入 50% SYN 丢包，真实往返 0 ms）：

| 超时 | 成功 | 成功读数中位 | ≥900 ms 的假读数 |
|---|---|---|---|
| 5 秒（旧） | 26/30 | **1010 ms** | 14 个 |
| 0.9 秒（新） | 10/30 | **0 ms** | 0 个 |

旧超时在一条零延迟的链路上报出 1010 毫秒，新超时报出真值和丢包。

### 否决过的做法：超时之后重测

另一条路是把超时留得很宽，读数超过 1000 ms 时重测几次，若重测明显变快（差值 > 800 ms）就判定中间
发生过重传，把这次记为失败。结论和现在一致，但代价是每个可疑样本多花最多 3 次握手，而且重测测的是
「后来那一刻」，不是原本要问的那一刻。

截断超时拿到同样的结果，不额外发包，逻辑只是一个常量。

## 怎么验证

`collect.rs` 里的 `crosscheck` 测试模块自己去跑 `free` 和 `df` 并比对，不用人工核对：

```bash
cargo test crosscheck -- --nocapture     # 仍然打印，同时会断言
```

内存和硬盘容差 64 MiB，留给两次取值之间机器自己的变化，而它比任何一种错口径都小一个数量级：本机
`df` 排除的 root 预留是 3.2 GiB，sysinfo 会记成已用的 page cache 是 2.4 GiB，htop 公式的偏差
288 MiB，全都远在容差之外。差到几百 MB 就是口径错了，不是抖动。

swap 单独用 1 MiB，理由见 swap 一节：它要抓的偏差只有几 MB，用 64 MiB 会整个放过去。
**每加一个上报字段，就问一句它能不能进这里**——swap 从写下的第一天就是错的，能一直错下去的唯一
原因是这个比对没带上它。

**这个比对必须打真机**：构造一段 `/proc/meminfo` 文本只能证明算术没写错，证明不了读的是对的字段
——而读错字段正是这个 agent 要修的原始 bug。用 `f_bavail` 代替 `f_bfree` 在构造数据上一样能算出
自洽的结果，只有跟 `df` 一比才露馅。

系统上没有 `free` 或 `df` 时测试**失败**，不是跳过：可以静默跳过比对的测试等于没有。

最近一次实机对照（Debian 12，3.8 GiB 内存，59 GiB 盘）：

| | monitor | 系统工具 |
|---|---|---|
| 内存 | 1.01 GiB / 3.82 GiB | `free` 1.02 / 3.82 |
| 硬盘 | 11.83 GiB / 58.94 GiB | `df` 11.83 / 58.94 |
| swap | 31.4 MiB / 1.00 GiB | `free` 31.4 / 1.00 |

同一次对照里另外几个字段（都不在 crosscheck 里，因为没有一条 `free` / `df` 那样的权威口径可比）：
`procs` 114 对 `ps -e` 110、`tcp` 64 对 `ss -t -a` 65、`udp` 4 对 4、`uptime` 1964951 对 1964971、
`load` 0.25/0.57/0.55 对 0.40/0.58/0.55——差的都是两次取值之间的时间，不是口径。

## 不要引入 sysinfo

agent 只跑 Linux（见 hub 仓库的
[decisions.md](https://github.com/stqfdyr/monitor/blob/main/docs/decisions.md)），直接读 `/proc`
更准、更小、更好懂。sysinfo 的内存和硬盘口径就是这篇文档在修的东西，加回来等于把 bug 加回来。
