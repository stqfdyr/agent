//! Linux-only metric collection, straight from /proc and statvfs.
//! No sysinfo: it gets memory and disk wrong for a probe (see docs/data-accuracy.md).

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use serde::Serialize;

/// Interface name prefixes never counted as real traffic.
const SKIP_IFACES: &[&str] = &[
    "lo", "docker", "veth", "br-", "virbr", "vmbr", "tap", "tun", "cni", "flannel", "podman", "fwbr", "fwpr",
    "kube", "cali", "nerdctl", "zt",
];

/// Pseudo/virtual filesystems that must not count toward disk totals.
const SKIP_FSTYPES: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "devpts",
    "mqueue",
    "hugetlbfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "autofs",
    "squashfs",
    "ramfs",
    "efivarfs",
    "nsfs",
    "overlay",
    "fuse.lxcfs",
    "rpc_pipefs",
];

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Facts {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub arch: String,
    pub virt: String,
    pub cpu_name: String,
    pub cpu_cores: u32,
    pub mem_total: u64,
    pub swap_total: u64,
    pub disk_total: u64,
    pub agent_version: String,
    /// The box's own addresses. The hub only sees whichever family the agent
    /// happened to connect over, and on a dual-stack box that is usually v6.
    pub ipv4: String,
    pub ipv6: String,
}

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Metrics {
    /// Identifies this boot. Changes on reboot, which is how the hub knows the
    /// kernel's byte counters restarted at zero.
    pub boot_id: String,
    pub uptime: u64,
    pub cpu: f32,
    pub load: [f32; 3],
    pub mem_total: u64,
    pub mem_used: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    /// Kernel lifetime byte counters. The hub accumulates these; the agent
    /// stores nothing and never tries to survive a reboot itself.
    pub net_rx_total: u64,
    pub net_tx_total: u64,
    pub net_rx: u64,
    pub net_tx: u64,
    pub tcp: u32,
    pub udp: u32,
    pub procs: u32,
}

#[derive(Default)]
pub struct Collector {
    prev_cpu: Option<(u64, u64)>,
    prev_net: Option<(Instant, u64, u64)>,
}

impl Collector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn facts(&self) -> Facts {
        let (v4, v6) = addresses();
        let mem = meminfo();
        let (cpu_name, cpu_cores) = cpuinfo();
        let (disk_total, _) = disk_usage(&real_mount_points());
        Facts {
            hostname: read_trim("/proc/sys/kernel/hostname").unwrap_or_else(|| "unknown".into()),
            os: os_pretty_name(),
            kernel: read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into()),
            arch: std::env::consts::ARCH.into(),
            virt: virtualization(),
            cpu_name,
            cpu_cores,
            mem_total: mem.get("MemTotal").copied().unwrap_or(0),
            swap_total: mem.get("SwapTotal").copied().unwrap_or(0),
            disk_total,
            agent_version: env!("CARGO_PKG_VERSION").into(),
            ipv4: v4,
            ipv6: v6,
        }
    }

    pub fn collect(&mut self) -> Metrics {
        let mem = meminfo();
        let (mem_total, mem_used) = mem_used(&mem);
        let (swap_total, swap_used) = swap_used(&mem);
        let (disk_total, disk_used) = disk_usage(&real_mount_points());
        let (rx_total, tx_total) = net_totals();
        let (rx, tx) = self.net_rate(rx_total, tx_total, Instant::now());
        let (tcp, udp) = conn_counts();

        Metrics {
            boot_id: read_trim("/proc/sys/kernel/random/boot_id").unwrap_or_default(),
            uptime: uptime(),
            cpu: self.cpu_percent(),
            load: loadavg(),
            mem_total,
            mem_used,
            swap_total,
            swap_used,
            disk_total,
            disk_used,
            net_rx_total: rx_total,
            net_tx_total: tx_total,
            net_rx: rx,
            net_tx: tx,
            tcp,
            udp,
            procs: proc_count(),
        }
    }

    /// CPU busy share since the previous call. First call has no baseline and
    /// reports 0 rather than a meaningless since-boot average.
    fn cpu_percent(&mut self) -> f32 {
        let Some((total, idle)) = cpu_jiffies() else {
            return 0.0;
        };
        let pct = match self.prev_cpu {
            Some((pt, pi)) if total > pt => {
                let dt = (total - pt) as f32;
                let di = idle.saturating_sub(pi) as f32;
                ((dt - di) / dt * 100.0).clamp(0.0, 100.0)
            }
            _ => 0.0,
        };
        self.prev_cpu = Some((total, idle));
        pct
    }

    fn net_rate(&mut self, rx: u64, tx: u64, now: Instant) -> (u64, u64) {
        let rate = match self.prev_net {
            Some((t, prx, ptx)) => {
                let secs = now.saturating_duration_since(t).as_secs_f64();
                if secs <= 0.0 {
                    (0, 0)
                } else {
                    // A backwards counter means a reboot or a wrap; report no
                    // spike rather than a garbage number.
                    (
                        (rx.saturating_sub(prx) as f64 / secs) as u64,
                        (tx.saturating_sub(ptx) as f64 / secs) as u64,
                    )
                }
            }
            None => (0, 0),
        };
        self.prev_net = Some((now, rx, tx));
        rate
    }
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// Parses /proc/meminfo into bytes keyed by field name.
fn meminfo() -> HashMap<String, u64> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo").unwrap_or_default())
}

fn parse_meminfo(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let (key, rest) = line.split_once(':')?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((key.to_owned(), kb * 1024))
        })
        .collect()
}

/// `free(1)`'s used column: total minus the kernel's own MemAvailable
/// estimate. sysinfo's `used_memory()` counts page cache as used and reads
/// gigabytes too high on any box that has been up for a while.
fn mem_used(m: &HashMap<String, u64>) -> (u64, u64) {
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let total = g("MemTotal");
    if total == 0 {
        return (0, 0);
    }
    let available = match g("MemAvailable") {
        0 => g("MemFree") + g("Buffers") + g("Cached"), // pre-3.14 kernels
        v => v,
    };
    (total, total.saturating_sub(available))
}

fn swap_used(m: &HashMap<String, u64>) -> (u64, u64) {
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let total = g("SwapTotal");
    let used = total.saturating_sub(g("SwapFree") + g("SwapCached"));
    (total, used.min(total))
}

fn cpu_jiffies() -> Option<(u64, u64)> {
    parse_cpu_jiffies(&fs::read_to_string("/proc/stat").ok()?)
}

fn parse_cpu_jiffies(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().next()?.strip_prefix("cpu ")?;
    let v: Vec<u64> = line.split_whitespace().filter_map(|f| f.parse().ok()).collect();
    if v.len() < 5 {
        return None;
    }
    // idle + iowait are both time the CPU was not doing work. guest and
    // guest_nice are already counted inside user and nice, so the sum stops
    // before them rather than charging that time twice.
    Some((v.iter().take(8).sum(), v[3] + v[4]))
}

fn loadavg() -> [f32; 3] {
    let text = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut it = text.split_whitespace();
    let mut next = || it.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    [next(), next(), next()]
}

fn uptime() -> u64 {
    fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|t| t.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0) as u64
}

/// The first real address of each family the kernel reports. On a VPS these
/// are the public ones; behind NAT the v4 is private, which is the honest
/// answer for what this machine actually holds — nothing is asked of any
/// outside service.
///
/// Interfaces are filtered by the same prefix list that keeps virtual devices
/// out of the traffic counters, so a docker bridge cannot be mistaken for the
/// machine's address.
fn addresses() -> (String, String) {
    let (mut v4, mut v6) = (String::new(), String::new());
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if skip_iface(&iface.name) || iface.is_link_local() || !iface.is_oper_up() {
            continue;
        }
        match iface.ip() {
            std::net::IpAddr::V4(ip) if v4.is_empty() => v4 = ip.to_string(),
            std::net::IpAddr::V6(ip) if v6.is_empty() => v6 = ip.to_string(),
            _ => {}
        }
    }
    (v4, v6)
}

/// Sums the kernel's lifetime byte counters over real interfaces.
fn net_totals() -> (u64, u64) {
    parse_net_dev(&fs::read_to_string("/proc/net/dev").unwrap_or_default())
}

fn parse_net_dev(text: &str) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in text.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else { continue };
        let name = name.trim();
        if skip_iface(name) {
            continue;
        }
        let f: Vec<u64> = rest.split_whitespace().filter_map(|v| v.parse().ok()).collect();
        if f.len() >= 9 {
            rx = rx.saturating_add(f[0]);
            tx = tx.saturating_add(f[8]);
        }
    }
    (rx, tx)
}

fn skip_iface(name: &str) -> bool {
    SKIP_IFACES.iter().any(|p| name.starts_with(p))
}

/// A pseudo filesystem, either named outright or as a flavour of one the list
/// holds — `fuse.lxcfs` and the like. Matched against the line rather than by
/// building `"{s}."` per candidate, which was a few hundred throwaway strings
/// every second on a box with a normal number of mounts.
fn skip_fstype(fstype: &str) -> bool {
    SKIP_FSTYPES.iter().any(|s| fstype == *s || fstype.strip_prefix(s).is_some_and(|r| r.starts_with('.')))
}

/// Mount points backed by something real, deduplicated by source device so a
/// bind mount or a second subvolume cannot double-count the same disk.
///
/// Re-read on every sample rather than cached at startup: a disk attached
/// later would otherwise stay invisible until the agent was restarted, and
/// /proc/self/mounts is a few kilobytes.
fn real_mount_points() -> Vec<String> {
    parse_mounts(&fs::read_to_string("/proc/self/mounts").unwrap_or_default())
}

fn parse_mounts(text: &str) -> Vec<String> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let (dev, mount, fstype) = (f[0], f[1], f[2]);
        if skip_fstype(fstype) {
            continue;
        }
        if !dev.starts_with('/') && fstype != "zfs" && fstype != "btrfs" {
            continue;
        }
        // ZFS datasets and btrfs subvolumes share one pool's free space.
        let key = dev.split('/').next().filter(|_| fstype == "zfs").unwrap_or(dev).to_owned();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(mount.replace("\\040", " "));
    }
    out
}

/// `used = total - free`, exactly what df reports. Scout used
/// `total - available`, which charges ext4's 5% root reserve to the user and
/// shows a fresh disk as several percent full.
fn disk_usage(mounts: &[String]) -> (u64, u64) {
    let mut total = 0u64;
    let mut used = 0u64;
    for m in mounts {
        let Ok(s) = rustix::fs::statvfs(m.as_str()) else { continue };
        let bs = if s.f_frsize > 0 { s.f_frsize } else { s.f_bsize };
        total = total.saturating_add(s.f_blocks.saturating_mul(bs));
        used = used.saturating_add(s.f_blocks.saturating_sub(s.f_bfree).saturating_mul(bs));
    }
    (total, used)
}

/// Socket counts from /proc/net/sockstat, which is a handful of short lines.
/// Counting lines in /proc/net/tcp meant reading and parsing the entire
/// connection table once a second — hundreds of kilobytes on a busy box, for
/// a number the kernel already keeps. TIME_WAIT sockets live in the v4 `tw`
/// counter for both families, so they are added once.
fn conn_counts() -> (u32, u32) {
    parse_sockstat(
        &fs::read_to_string("/proc/net/sockstat").unwrap_or_default(),
        &fs::read_to_string("/proc/net/sockstat6").unwrap_or_default(),
    )
}

fn parse_sockstat(v4: &str, v6: &str) -> (u32, u32) {
    let stat = |text: &str, prefix: &str, key: &str| {
        text.lines()
            .find_map(|line| {
                let mut fields = line.strip_prefix(prefix)?.split_whitespace();
                while let Some(word) = fields.next() {
                    if word == key {
                        return fields.next()?.parse::<u32>().ok();
                    }
                }
                None
            })
            .unwrap_or(0)
    };
    (
        stat(v4, "TCP:", "inuse") + stat(v4, "TCP:", "tw") + stat(v6, "TCP6:", "inuse"),
        stat(v4, "UDP:", "inuse") + stat(v6, "UDP6:", "inuse"),
    )
}

fn proc_count() -> u32 {
    fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
                .count() as u32
        })
        .unwrap_or(0)
}

fn cpuinfo() -> (String, u32) {
    let text = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let name = text
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            matches!(k.trim(), "model name" | "Model" | "cpu model").then(|| v.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into());
    let cores = text.lines().filter(|l| l.starts_with("processor")).count().max(1) as u32;
    (name, cores)
}

fn os_pretty_name() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| Some(l.strip_prefix("PRETTY_NAME=")?.trim_matches('"').to_owned()))
        })
        .unwrap_or_else(|| "Linux".into())
}

fn virtualization() -> String {
    if fs::metadata("/proc/vz").is_ok() {
        return "openvz".into();
    }
    if fs::metadata("/proc/xen").is_ok() {
        return "xen".into();
    }
    if fs::metadata("/.dockerenv").is_ok() {
        return "docker".into();
    }
    if let Some(t) = read_trim("/sys/hypervisor/type") {
        return t.to_lowercase();
    }
    for path in ["/sys/class/dmi/id/product_name", "/sys/class/dmi/id/sys_vendor"] {
        let Some(v) = read_trim(path) else { continue };
        let l = v.to_lowercase();
        for k in ["kvm", "vmware", "virtualbox", "qemu", "hyper-v", "xen", "bochs", "amazon", "google"] {
            if l.contains(k) {
                return k.into();
            }
        }
    }
    if fs::read_to_string("/proc/cpuinfo").is_ok_and(|t| t.contains("hypervisor")) {
        "vm".into()
    } else {
        "none".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_matches_free_not_sysinfo() {
        // Real /proc/meminfo from a 3.8 GiB box holding 2.5 GiB of page cache.
        let m = parse_meminfo(
            "MemTotal:        4008884 kB\nMemFree:          602756 kB\nMemAvailable:    2947484 kB\n\
             Buffers:          129100 kB\nCached:          2351560 kB\nSReclaimable:     154008 kB\n\
             Shmem:              2176 kB\nSwapTotal:       1048572 kB\nSwapFree:         987264 kB\n\
             SwapCached:        13280 kB\n",
        );
        let (total, used) = mem_used(&m);
        assert_eq!(total, 4008884 * 1024);
        assert_eq!(used, (4008884 - 2947484) * 1024, "must match the `free` used column");
        // The bug this replaces: counting cache as used reported ~3.3 GiB here.
        assert!(used < (total - g_cached(&m)), "page cache must not count as used");

        let (st, su) = swap_used(&m);
        assert_eq!(st, 1048572 * 1024);
        assert_eq!(su, (1048572 - 987264 - 13280) * 1024);
    }

    fn g_cached(m: &HashMap<String, u64>) -> u64 {
        m.get("Cached").copied().unwrap_or(0)
    }

    #[test]
    fn memory_falls_back_when_memavailable_is_absent() {
        let m = parse_meminfo("MemTotal: 1000 kB\nMemFree: 200 kB\nBuffers: 100 kB\nCached: 300 kB\n");
        assert_eq!(mem_used(&m), (1000 * 1024, 400 * 1024));
        assert_eq!(mem_used(&HashMap::new()), (0, 0));
    }

    #[test]
    fn cpu_percent_needs_a_baseline_then_uses_deltas() {
        assert_eq!(parse_cpu_jiffies("cpu  40 0 35 925 0 0 0 0 0 0\n"), Some((1000, 925)));
        // The last two columns are guest and guest_nice, which the kernel has
        // already counted inside user and nice: 80 busy jiffies, not 1080.
        assert_eq!(parse_cpu_jiffies("cpu  10 10 10 10 10 10 10 10 500 500\n"), Some((80, 20)));
        assert!(parse_cpu_jiffies("garbage").is_none());

        // 100 more jiffies since the baseline, 25 of them idle => 75% busy.
        let mut c = Collector::new();
        c.prev_cpu = Some((1000, 925));
        let busy = |(total, idle): (u64, u64), prev: (u64, u64)| {
            let dt = (total - prev.0) as f32;
            ((dt - (idle - prev.1) as f32) / dt * 100.0).clamp(0.0, 100.0)
        };
        assert_eq!(busy((1100, 950), c.prev_cpu.unwrap()), 75.0);
        // A live box has no baseline on the first call, so it reports 0.
        assert_eq!(Collector::new().cpu_percent(), 0.0);
    }

    #[test]
    fn socket_counts_come_from_sockstat_not_the_connection_table() {
        let v4 = "sockets: used 226\nTCP: inuse 78 orphan 1 tw 7 alloc 85 mem 119\nUDP: inuse 2 mem 150\n";
        let v6 = "TCP6: inuse 1\nUDP6: inuse 4\n";
        // Matches what counting lines in /proc/net/tcp{,6} reported on this box:
        // TIME_WAIT sockets are held in the v4 `tw` field for both families.
        assert_eq!(parse_sockstat(v4, v6), (86, 6));
        assert_eq!(parse_sockstat("", ""), (0, 0));
    }

    #[test]
    fn net_skips_virtual_interfaces() {
        let dev = "Inter-|   Receive\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n\
                   eth0: 1000 1 0 0 0 0 0 0 2000 2 0 0 0 0 0 0\n\
                     lo: 9999 1 0 0 0 0 0 0 9999 2 0 0 0 0 0 0\n\
              docker0: 5555 1 0 0 0 0 0 0 5555 2 0 0 0 0 0 0\n\
                  wg0: 300 1 0 0 0 0 0 0 400 2 0 0 0 0 0 0\n";
        assert_eq!(parse_net_dev(dev), (1300, 2400));
    }

    #[test]
    fn net_rate_is_zero_on_first_sample_and_after_a_reboot() {
        let mut c = Collector::new();
        let t0 = Instant::now();
        assert_eq!(c.net_rate(1000, 2000, t0), (0, 0));
        let t1 = t0 + std::time::Duration::from_secs(2);
        assert_eq!(c.net_rate(1200, 2400, t1), (100, 200));
        // Counter restarted: no negative, no bogus spike.
        let t2 = t1 + std::time::Duration::from_secs(2);
        assert_eq!(c.net_rate(50, 60, t2), (0, 0));
    }

    /// Two independent guards drop a mount: its filesystem type, and whether
    /// its source looks like a device. Most junk trips both, which means either
    /// one can be deleted without a row moving -- so the fixture carries a line
    /// that only one of them catches.
    #[test]
    fn mounts_drop_pseudo_filesystems_and_duplicate_devices() {
        let mounts = parse_mounts(
            "/dev/vda1 / ext4 rw 0 0\n\
             proc /proc proc rw 0 0\n\
             tmpfs /run tmpfs rw 0 0\n\
             /dev/loop0 /snap/core24/1 squashfs ro 0 0\n\
             none /mnt/scratch ext4 rw 0 0\n\
             /dev/vda1 /var/lib/bind ext4 rw 0 0\n\
             overlay /var/lib/docker/overlay2/x/merged overlay rw 0 0\n\
             /dev/vdb1 /data xfs rw 0 0\n\
             tank/set1 /tank zfs rw 0 0\n\
             tank/set2 /tank/sub zfs rw 0 0\n",
        );
        // /snap/... is a real device holding a pseudo filesystem: only the
        // fstype list rejects it, and a squashfs per snap would otherwise add a
        // full copy of each one to the machine's disk total.
        // /mnt/scratch is the mirror image -- a real filesystem whose source is
        // not a path -- and only the device check rejects that.
        assert_eq!(mounts, vec!["/", "/data", "/tank"]);
    }

    #[test]
    fn real_host_collection_is_sane() {
        let mut c = Collector::new();
        let f = c.facts();
        assert!(!f.hostname.is_empty() && f.cpu_cores >= 1 && f.mem_total > 0);
        // Whatever this box has must parse; a virtual bridge must not win.
        assert!(f.ipv4.is_empty() || f.ipv4.parse::<std::net::Ipv4Addr>().is_ok());
        assert!(f.ipv6.is_empty() || f.ipv6.parse::<std::net::Ipv6Addr>().is_ok());
        assert!(!f.ipv4.is_empty() || !f.ipv6.is_empty(), "a reachable box has at least one address");
        // The prefix filter is the only thing keeping a docker bridge out.
        assert!(!f.ipv4.starts_with("172.17."), "a virtual bridge is not this machine's address");
        let m = c.collect();
        assert!(!m.boot_id.is_empty(), "boot_id drives reboot detection");
        assert!(m.mem_used > 0 && m.mem_used < m.mem_total);
        assert!(m.disk_used <= m.disk_total && m.disk_total > 0);
        assert!((0.0..=100.0).contains(&m.cpu));
    }
}

#[cfg(test)]
mod crosscheck {
    use super::*;

    /// The first of the two rules, checked against the tools it names rather
    /// than printed for a human to check. Constructed /proc text can only show
    /// the arithmetic is right; it cannot show the right field was read, and
    /// reading the wrong field is the bug this agent exists to fix.
    ///
    /// The tolerance covers the machine moving between the two readings, and it
    /// is an order of magnitude below every wrong answer: on this box the root
    /// reserve `df` excludes is gigabytes, and the page cache sysinfo counts as
    /// used is gigabytes. Sixty-four mebibytes tells them apart with room over.
    ///
    /// Still prints, so `cargo test crosscheck -- --nocapture` reads the same.
    #[test]
    fn memory_and_disk_agree_with_free_and_df_on_this_machine() {
        let mut c = Collector::new();
        let m = c.collect();
        let gib = |b: u64| b as f64 / 1024.0 / 1024.0 / 1024.0;
        println!("mem  used={:.2}G total={:.2}G", gib(m.mem_used), gib(m.mem_total));
        println!("disk used={:.2}G total={:.2}G", gib(m.disk_used), gib(m.disk_total));
        println!("net  rx_total={} tx_total={}", m.net_rx_total, m.net_tx_total);

        const TOLERANCE: u64 = 64 * 1024 * 1024;
        let close = |ours: u64, theirs: u64, what: &str| {
            let drift = ours.abs_diff(theirs);
            assert!(drift < TOLERANCE, "{what}: ours={ours} theirs={theirs} drift={drift}");
        };

        // free(1) row "Mem:": total, used, ... -- its used column is
        // total - available, the same question MemAvailable answers.
        let free = tool("free", &["-b"]);
        let mut row = free.lines().nth(1).expect("free prints a Mem: row").split_whitespace().skip(1);
        let parse = |v: Option<&str>| v.expect("free column").parse::<u64>().expect("a byte count");
        assert_eq!(m.mem_total, parse(row.next()), "MemTotal is not free's total");
        close(m.mem_used, parse(row.next()), "memory");

        // df(1) counts the root reserve as free, which is f_bfree and not
        // f_bavail. Compared against one filesystem, because df is being asked
        // about one and the metric sums every mount.
        let (disk_total, disk_used) = disk_usage(&["/".to_owned()]);
        let df = tool("df", &["-B1", "--output=size,used", "/"]);
        let mut row = df.lines().nth(1).expect("df prints a data row").split_whitespace();
        let parse = |v: Option<&str>| v.expect("df column").parse::<u64>().expect("a byte count");
        assert_eq!(disk_total, parse(row.next()), "f_blocks is not df's size");
        close(disk_used, parse(row.next()), "disk");
    }

    /// Missing tools are a failure, not a reason to pass quietly: this test is
    /// worth nothing if it can skip the comparison it exists for.
    fn tool(program: &str, args: &[&str]) -> String {
        let out = std::process::Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap_or_else(|e| panic!("{program}(1) is what these numbers are checked against: {e}"));
        assert!(out.status.success(), "{program} exited with {}", out.status);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}
