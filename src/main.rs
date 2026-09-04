//! monitor-agent: reports one Linux host to a monitor hub over WebSocket.

mod collect;

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{Sink, SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use collect::Collector;

struct Args {
    server: String,
    token: String,
    interval: u64,
    /// Set to talk to a hub that only answers plain HTTP -- a hub reached at
    /// ip:port with no TLS in front. Off by default, because the token then
    /// travels in the clear.
    insecure: bool,
}

fn usage() -> ! {
    eprintln!(
        "monitor-agent {}\n\n\
         Usage: monitor-agent --server <url> --token <token> [options]\n\n\
         Options:\n  \
           --server <url>       Hub base URL, e.g. https://hub.example.com\n  \
           --token <token>      Node token from the hub panel\n  \
           --interval <secs>    Report interval (default 1)\n  \
           --insecure           Allow plain ws:// to a remote hub; the token\n  \
                                travels in the clear. Only for a hub reached\n  \
                                at ip:port with no TLS in front.\n",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn parse_args() -> Result<Args> {
    let (mut server, mut token, mut interval, mut insecure) = (None, None, 1u64, false);
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--server" => server = Some(value()),
            "--token" => token = Some(value()),
            "--interval" => interval = value().parse().unwrap_or_else(|_| usage()),
            "--insecure" => insecure = true,
            "-h" | "--help" => usage(),
            other => bail!("unknown argument: {other}"),
        }
    }
    let server = server.or_else(|| std::env::var("MONITOR_SERVER").ok()).unwrap_or_else(|| usage());
    let token = token.or_else(|| std::env::var("MONITOR_TOKEN").ok()).unwrap_or_else(|| usage());
    Ok(Args { server, token, interval: interval.clamp(1, 3600), insecure })
}

/// `https://host/path` -> `wss://host/path/api/agent/ws`. The token travels in
/// an Authorization header rather than the query string, so it stays out of
/// reverse-proxy access logs.
///
/// `insecure` is the operator saying the hub has no TLS -- a default ip:port
/// deployment. It both allows plain ws:// to a remote hub and stops a bare
/// host from being upgraded to TLS the hub does not speak; without the second
/// half the flag would connect to a port that never completes a handshake.
fn ws_url(server: &str, insecure: bool) -> Result<String> {
    let base = server.trim_end_matches('/');
    let scheme = if insecure { "ws" } else { "wss" };
    let base = match base.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        Some(("wss" | "ws", _)) => base.to_owned(),
        _ => format!("{scheme}://{base}"),
    };
    if base.starts_with("ws://") && !insecure && !is_loopback(&base) {
        bail!(
            "refusing plaintext ws:// to a remote hub; the token would travel in the clear. \
             Pass --insecure if that hub really has no TLS"
        );
    }
    Ok(format!("{base}/api/agent/ws"))
}

/// IPv6 literals are bracketed, so the port cannot be split off at the first
/// colon.
///
/// The host is parsed rather than prefix-matched: `127.attacker.example` is a
/// name someone else resolves, and it begins with the loopback net. Anything
/// that is not a literal loopback address falls to the plaintext refusal --
/// including `::ffff:127.0.0.1`, which stays behind `--insecure`.
fn is_loopback(url: &str) -> bool {
    let authority = url.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.parse::<std::net::IpAddr>().map_or(host == "localhost", |ip| ip.is_loopback())
}

#[derive(Deserialize)]
struct Rpc {
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Deserialize, Clone, Debug)]
struct PingTask {
    id: i64,
    target: String,
    interval: u64,
}

fn notify(method: &str, params: serde_json::Value) -> Message {
    Message::Text(
        serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string().into(),
    )
}

/// A write with a deadline, given the same budget as the silence it would
/// otherwise hide.
///
/// Reads and writes share one `select!` loop, so a socket that never drains
/// stops everything: the watchdog below stops being polled, and the frame that
/// would prove the path alive is never read. The kernel gives up on such a
/// socket after tcp_retries2, about a quarter of an hour.
///
/// A timed-out write leaves a half-written frame in the stream. Every caller
/// ends the session on the error, so that frame dies with the socket.
async fn send(ws: &mut (impl Sink<Message, Error = WsError> + Unpin), m: Message) -> Result<()> {
    tokio::time::timeout(HUB_SILENCE, ws.send(m))
        .await
        .map_err(|_| anyhow!("write stalled for {}s", HUB_SILENCE.as_secs()))?
        .context("write")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let url = ws_url(&args.server, args.insecure)?;
    let mut collector = Collector::new();
    let mut wait = 0u64;

    loop {
        let started = Instant::now();
        if let Err(e) = session(&url, &args.token, &mut collector, args.interval).await {
            eprintln!("session ended: {e:#}");
        }
        wait = reconnect_wait(wait, started.elapsed());
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

/// How long to wait before reconnecting, from the previous wait and how long
/// the session that just ended had lasted.
///
/// A session that reported for a while proves the hub reachable and the token
/// good, so whatever ended it was a restart or a network blink -- one second,
/// not the minute a dead endpoint has earned. Only sessions that die young keep
/// doubling, which is what keeps an agent off a hub in a crash loop.
fn reconnect_wait(previous: u64, lasted: Duration) -> u64 {
    if lasted >= Duration::from_secs(30) {
        1
    } else {
        (previous * 2).clamp(1, 60)
    }
}

/// Deadline for getting a connection up, covering all three stages of it.
///
/// Only the TCP handshake has a deadline of its own; the TLS exchange and the
/// HTTP upgrade have none. A peer that accepts and then says nothing leaves
/// `connect_async` awaiting forever, and with it an agent still running, no
/// longer reporting, with nothing in its log to say so.
///
/// Generous on purpose: a healthy connect takes a quarter of a second and the
/// slowest measured was sixty. This is not a latency budget, it is the line
/// past which nothing is coming.
const CONNECT_DEADLINE: Duration = Duration::from_secs(120);

/// The hub sends one kind of message, a probe list a few hundred bytes long.
/// Left at its default tungstenite accepts 64 MiB of it -- the whole memory
/// budget the unit file grants this process, handed to the other end.
const MAX_MESSAGE: usize = 64 * 1024;

/// How long the agent waits for any frame from the hub before giving up.
///
/// The hub pings every 30 seconds and drops an agent quiet for 120. Without a
/// mirror of that, a path failing in one direction only -- a NAT entry
/// expiring, a route going dark -- leaves the agent writing into a socket the
/// kernel retransmits on for a quarter of an hour, long after the panel has
/// called the node offline.
///
/// Landing under the hub's own timeout makes the agent give up first, so a
/// one-way failure costs this constant instead of tcp_retries2.
const HUB_SILENCE: Duration = Duration::from_secs(90);

/// One connection: say hello, then report until the socket dies.
async fn session(url: &str, token: &str, collector: &mut Collector, interval: u64) -> Result<()> {
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().context("token is not header-safe")?);
    let config =
        WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
    let connect = tokio_tungstenite::connect_async_with_config(request, Some(config), false);
    let (mut ws, _) = tokio::time::timeout(CONNECT_DEADLINE, connect)
        .await
        .with_context(|| format!("no connection after {}s", CONNECT_DEADLINE.as_secs()))?
        .context("connect")?;
    eprintln!("connected");

    send(&mut ws, notify("hello", serde_json::to_value(collector.facts())?)).await?;

    let (result_tx, mut result_rx) = mpsc::channel::<Message>(64);
    let mut ping_tasks: Vec<(PingTask, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(interval));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_frame = Instant::now();

    let result = loop {
        tokio::select! {
            _ = ticker.tick() => {
                let m = serde_json::to_value(collector.collect())?;
                if let Err(e) = send(&mut ws, notify("report", m)).await { break Err(e); }
            }
            // Built afresh each pass from the last frame, so silence costs
            // exactly HUB_SILENCE. A timer polled every HUB_SILENCE / 3 spent
            // a whole extra tick before firing, which put the worst case at
            // the hub's own timeout instead of under it. Not the report tick:
            // --interval can stretch that to an hour.
            _ = tokio::time::sleep(HUB_SILENCE.saturating_sub(last_frame.elapsed())) => {
                break Err(anyhow!("no frame from the hub in {}s", HUB_SILENCE.as_secs()));
            }
            Some(msg) = result_rx.recv() => {
                if let Err(e) = send(&mut ws, msg).await { break Err(e); }
            }
            incoming = ws.next() => {
                // Any frame proves the path is still there, the hub's
                // heartbeat ping included -- the only one that arrives on an
                // otherwise idle connection.
                last_frame = Instant::now();
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(rpc) = serde_json::from_str::<Rpc>(&text) {
                            if rpc.method == "ping.tasks" {
                                if let Ok(tasks) = serde_json::from_value::<Vec<PingTask>>(rpc.params) {
                                    respawn_ping_tasks(&mut ping_tasks, tasks, &result_tx);
                                }
                            }
                        }
                    }
                    // Ping included: tungstenite queues the pong itself and
                    // writes it on the next read. A manual reply replaces that
                    // queued frame with a copy of itself.
                    Some(Ok(_)) => {}
                    Some(Err(e)) => break Err(e.into()),
                    None => break Ok(()),
                }
            }
        }
    };

    for (_, handle) in ping_tasks {
        handle.abort();
    }
    result
}

/// How many probe loops the hub may have running at once.
///
/// A task serialises to about forty bytes, so one [`MAX_MESSAGE`] frame asks
/// for some fifteen hundred of them; at the five-second floor below that is a
/// few hundred outbound connects a second, to addresses the hub chose. The
/// agent runs on someone else's VPS, where that reads as a port scan and works
/// as an amplifier. A hub that is compromised or merely buggy is the whole
/// threat model, so the list gets a ceiling rather than trust.
const MAX_PING_TASKS: usize = 64;

/// Replaces the running probe loops with the hub's current task list, leaving
/// unchanged tasks alone so their timers survive a push.
fn respawn_ping_tasks(
    running: &mut Vec<(PingTask, tokio::task::JoinHandle<()>)>,
    mut wanted: Vec<PingTask>,
    tx: &mpsc::Sender<Message>,
) {
    if wanted.len() > MAX_PING_TASKS {
        // Dropping probes in silence is an hour of wondering which ones ran.
        eprintln!("hub asked for {} ping tasks, running {MAX_PING_TASKS}", wanted.len());
        wanted.truncate(MAX_PING_TASKS);
    }
    running.retain(|(task, handle)| {
        let keep =
            wanted.iter().any(|w| w.id == task.id && w.target == task.target && w.interval == task.interval);
        if !keep {
            handle.abort();
        }
        keep
    });
    for task in wanted {
        if running.iter().any(|(t, _)| t.id == task.id) {
            continue;
        }
        let (tx, spawned) = (tx.clone(), task.clone());
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(spawned.interval.clamp(5, 3600)));
            loop {
                ticker.tick().await;
                let latency = tcp_ping(&spawned.target).await;
                let msg =
                    notify("ping.result", serde_json::json!({"task_id": spawned.id, "latency_ms": latency}));
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
        });
        running.push((task, handle));
    }
}

/// Deadline for one handshake, deliberately under the kernel's first SYN
/// retransmit.
///
/// Linux arms its initial SYN timer at one second. Wait longer and a dropped
/// SYN does not fail, it succeeds late: `connect` hands back the retransmit
/// timer plus the round trip, which is a different measurement in the same
/// units.
///
/// Cutting the wait short means every reading that comes back belongs to a
/// handshake that completed on the first SYN, and a dropped one becomes -1. The
/// cost is that a link whose genuine round trip exceeds this reads unreachable.
const HANDSHAKE_DEADLINE: Duration = Duration::from_millis(900);

/// How many of a name's addresses one probe will try.
///
/// Every dead one costs a [`HANDSHAKE_DEADLINE`] before the next is reached,
/// and a probe may not outlast the five-second floor on its own interval.
const MAX_PING_ADDRS: usize = 3;

/// Round trip of a TCP handshake to the target, in milliseconds; -1 when no
/// address answered inside [`HANDSHAKE_DEADLINE`].
///
/// The name is resolved before the clock starts: `TcpStream::connect` on a
/// hostname resolves first and connects second, folding the resolver's latency
/// into every sample.
async fn tcp_ping(target: &str) -> i32 {
    let Ok(addresses) = tokio::net::lookup_host(target).await else { return -1 };
    handshake(addresses).await
}

/// The first address that completes a handshake, in milliseconds.
///
/// The clock restarts on each address, so a dead one contributes nothing to
/// the reading. Summing them would report the wait as latency, which is what
/// [`HANDSHAKE_DEADLINE`] exists to prevent.
///
/// Any failure moves on, refusals included. glibc hands back the v6 address
/// first, and on a box whose v6 has no route -- or a route into a black hole
/// -- stopping at the first address reads unreachable forever while the
/// target answers over v4. Which family failed is not worth an errno: a
/// refusal costs a millisecond, and the black hole is the case that has to
/// move on.
async fn handshake(addresses: impl Iterator<Item = std::net::SocketAddr>) -> i32 {
    for address in addresses.take(MAX_PING_ADDRS) {
        let started = std::time::Instant::now();
        if let Ok(Ok(_)) = tokio::time::timeout(HANDSHAKE_DEADLINE, TcpStream::connect(address)).await {
            return started.elapsed().as_millis().min(i32::MAX as u128) as i32;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hub_restart_costs_a_second_while_an_unreachable_one_still_backs_off() {
        // Nothing on the other end: double to the ceiling and stay there.
        let mut wait = 0;
        let climb: Vec<u64> = (0..8)
            .map(|_| {
                wait = reconnect_wait(wait, Duration::from_secs(1));
                wait
            })
            .collect();
        assert_eq!(climb, [1, 2, 4, 8, 16, 32, 60, 60]);

        // A session that ran starts over however high the wait had climbed:
        // the hub-restart case, where 60s of blank panel was the bug.
        assert_eq!(reconnect_wait(60, Duration::from_secs(3600)), 1);
        // Connected but dropped before it proved anything: still a retreat.
        assert_eq!(reconnect_wait(4, Duration::from_secs(29)), 8);
    }

    #[test]
    fn ws_url_upgrades_scheme_and_refuses_plaintext_to_remote() {
        assert_eq!(ws_url("https://hub.example.com/", false).unwrap(), "wss://hub.example.com/api/agent/ws");
        assert_eq!(ws_url("http://127.0.0.1:28080", false).unwrap(), "ws://127.0.0.1:28080/api/agent/ws");
        // A bare host defaults to TLS rather than leaking the token.
        assert!(ws_url("hub.example.com", false).unwrap().starts_with("wss://"));
        assert!(ws_url("http://hub.example.com", false).is_err());
        // Bracketed IPv6 loopback is not remote.
        assert_eq!(ws_url("http://[::1]:28080", false).unwrap(), "ws://[::1]:28080/api/agent/ws");
        assert_eq!(ws_url("http://localhost:28080", false).unwrap(), "ws://localhost:28080/api/agent/ws");
        // A name that merely starts like the loopback net is somebody else's
        // machine: the host is parsed, not prefix-matched.
        assert!(ws_url("http://127.attacker.example/", false).is_err());
        // Fail closed: a mapped literal is not read as loopback either.
        assert!(ws_url("http://[::ffff:127.0.0.1]:28080", false).is_err());
        // No token anywhere in the URL: it rides in a header instead.
        assert!(!ws_url("https://hub.example.com", false).unwrap().contains("token"));
    }

    /// `--insecure` is for a hub reached at ip:port with no TLS: it allows the
    /// plaintext hop, and a bare host stops meaning TLS -- otherwise the flag
    /// would dial wss:// at a port that cannot answer it.
    #[test]
    fn insecure_allows_plaintext_to_a_remote_hub_and_stops_upgrading_bare_hosts() {
        assert_eq!(
            ws_url("http://203.0.113.10:28080", true).unwrap(),
            "ws://203.0.113.10:28080/api/agent/ws"
        );
        assert_eq!(ws_url("203.0.113.10:28080", true).unwrap(), "ws://203.0.113.10:28080/api/agent/ws");
        // An explicit https:// hub stays on TLS: the flag permits plaintext,
        // it does not force it.
        assert_eq!(ws_url("https://hub.example.com", true).unwrap(), "wss://hub.example.com/api/agent/ws");
    }

    #[tokio::test]
    async fn tcp_ping_measures_success_and_reports_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        // A loopback handshake finishes inside a millisecond, so this reading
        // is 0 whether it is timed or not. What it pins is the contract either
        // side: reachable is non-negative, unreachable is -1. That the number
        // is a real round trip rests on the deadline test below.
        assert!(tcp_ping(&addr.to_string()).await >= 0);
        assert_eq!(tcp_ping("127.0.0.1:1").await, -1, "nothing is listening there");
        // A target that will not resolve comes back unreachable rather than
        // taking the probe down.
        assert_eq!(tcp_ping("127.0.0.1:99999").await, -1, "not a parseable target");

        // A name whose first address is dead: the shape of a dual-stack target
        // on a box whose v6 goes nowhere. The probe moves on instead of
        // calling a reachable target unreachable for good.
        let dead: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert!(handshake([dead, addr].into_iter()).await >= 0, "a dead address must not end the probe");
        assert_eq!(handshake([dead, dead].into_iter()).await, -1, "every address failed");
        // Past the ceiling the rest are not tried: a name with a long address
        // list would otherwise hold a probe past its own interval.
        assert_eq!(
            handshake([dead, dead, dead, addr].into_iter()).await,
            -1,
            "a fourth address is not tried"
        );
    }

    /// The deadline is the whole mechanism: it has to land under the kernel's
    /// first SYN retransmit, or a dropped SYN comes back as a ~1200ms "latency"
    /// that is really the 1s timer plus the round trip. Those readings cluster
    /// at 1200ms and 3200ms with nothing in between -- the signature of a
    /// retransmit, not a slow link.
    #[test]
    fn the_handshake_deadline_stays_under_the_kernels_syn_timer() {
        assert!(
            HANDSHAKE_DEADLINE < Duration::from_secs(1),
            "a deadline at or past the 1s initial RTO lets retransmits be reported as latency"
        );
    }

    /// The hub pings every 30s and drops an agent silent for 120s (its
    /// SILENCE). Both ends of this window are load-bearing and neither is
    /// visible from this file.
    ///
    /// This reads as the whole detection time only because the watchdog sleeps
    /// to a deadline. While it was an interval polled every HUB_SILENCE / 3,
    /// the worst case was 90 + 30 = 120s -- the hub's timeout exactly, so the
    /// property below was false while its assertion stayed green.
    #[test]
    fn the_agent_gives_up_on_a_silent_hub_before_the_hub_gives_up_on_it() {
        assert!(
            HUB_SILENCE < Duration::from_secs(120),
            "past the hub's own timeout the agent stops being what recovers the connection"
        );
        assert!(HUB_SILENCE > Duration::from_secs(60), "two lost heartbeats are a blip, not a dead link");
    }

    #[test]
    fn ping_tasks_keep_their_timers_unless_the_task_changed() {
        // The flavour the binary runs on, so this exercises the real thing.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _g = rt.enter();
        let (tx, _rx) = mpsc::channel(8);
        let mut running = Vec::new();
        let task = |id, target: &str, interval| PingTask { id, target: target.into(), interval };

        respawn_ping_tasks(&mut running, vec![task(1, "a:1", 60), task(2, "b:2", 60)], &tx);
        assert_eq!(running.len(), 2);
        let (first, second) = (running[0].1.id(), running[1].1.id());

        // Task 1 unchanged, task 2 retargeted, task 3 added.
        respawn_ping_tasks(
            &mut running,
            vec![task(1, "a:1", 60), task(2, "c:3", 60), task(3, "d:4", 60)],
            &tx,
        );
        assert_eq!(running.len(), 3);
        assert_eq!(running[0].1.id(), first, "unchanged task must not be restarted");
        // The other half of the rule: a task whose target moved is torn down,
        // or it goes on probing the old address.
        assert_ne!(running[1].1.id(), second, "a retargeted task must be restarted");

        // A hub that sends interval 0 must not take the probe down: tokio's
        // interval panics on a zero period, and a panicked task stops
        // reporting without saying so.
        respawn_ping_tasks(&mut running, vec![task(9, "e:5", 0)], &tx);
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(50)).await });
        assert!(!running[0].1.is_finished(), "a zero interval must be clamped, not panic the probe");

        // One 64 KiB frame is worth some fifteen hundred of these. The agent
        // runs what it will run and says so, rather than trusting the count.
        let flood = (0..500).map(|id| task(id, "f:6", 60)).collect();
        respawn_ping_tasks(&mut running, flood, &tx);
        assert_eq!(running.len(), MAX_PING_TASKS, "the hub does not choose how many probes run");
    }
}
