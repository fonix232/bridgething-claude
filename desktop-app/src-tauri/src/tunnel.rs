// Finds the connected Car Thing and keeps a reverse SSH tunnel to it alive.
// Rust port of mac/find-device.sh's subnet scan + mac/tunnel.sh's
// find-connect-rescan loop.
//
// The device's address on the USB gadget subnet isn't fixed, so this scans
// 10.42.1.0/24 for a host on port 22, confirms each hit with a real,
// non-interactive root SSH login (a port merely being open could be any
// host), then opens `ssh -N -R <port>:127.0.0.1:<port>` and blocks until
// that connection drops, then rescans. Runs for the lifetime of the app —
// there is no launchd KeepAlive backstop here, this loop *is* the backstop.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::Notify;

const SUBNET_PREFIX_DEFAULT: &str = "10.42.1";
const SSH_PORT: u16 = 22;
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
const RESCAN_DELAY: Duration = Duration::from_secs(3);
const BATCH_SIZE: u32 = 32;

fn subnet_prefix() -> String {
    std::env::var("CLAUDE_THING_SUBNET").unwrap_or_else(|_| SUBNET_PREFIX_DEFAULT.to_string())
}

fn known_hosts_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    PathBuf::from(home).join(".ssh/known_hosts_carthing")
}

async fn probe(ip: Ipv4Addr) -> bool {
    let addr = SocketAddr::new(IpAddr::V4(ip), SSH_PORT);
    matches!(tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await, Ok(Ok(_)))
}

async fn confirm_ssh(ip: &str) -> bool {
    let known_hosts = known_hosts_path();
    let status = Command::new("/usr/bin/ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=2", "-o", "StrictHostKeyChecking=accept-new"])
        .arg(format!("-oUserKnownHostsFile={}", known_hosts.display()))
        .arg(format!("root@{ip}"))
        .arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    matches!(status, Ok(s) if s.success())
}

// Sweeps the subnet in small batches (not all 254 at once) and confirms every
// port-22 hit with a real login before returning it.
async fn find_device() -> Option<String> {
    let prefix = subnet_prefix();
    let mut n: u32 = 1;
    while n <= 254 {
        let end = (n + BATCH_SIZE - 1).min(254);
        let mut set = tokio::task::JoinSet::new();
        for i in n..=end {
            let ip: Ipv4Addr = format!("{prefix}.{i}").parse().expect("valid octet");
            set.spawn(async move { (ip, probe(ip).await) });
        }
        let mut hits: Vec<Ipv4Addr> = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok((ip, true)) = res {
                hits.push(ip);
            }
        }
        hits.sort();
        for ip in hits {
            let ip = ip.to_string();
            if confirm_ssh(&ip).await {
                return Some(ip);
            }
        }
        n = end + 1;
    }
    None
}

async fn spawn_reverse_tunnel(ip: &str, port: u16) -> std::io::Result<tokio::process::Child> {
    let known_hosts = known_hosts_path();
    Command::new("/usr/bin/ssh")
        .arg("-N")
        .args(["-R", &format!("{port}:127.0.0.1:{port}")])
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args(["-o", "ServerAliveInterval=15"])
        .args(["-o", "ServerAliveCountMax=3"])
        .args(["-o", "ConnectTimeout=10"])
        .args(["-o", "StrictHostKeyChecking=accept-new"])
        .arg(format!("-oUserKnownHostsFile={}", known_hosts.display()))
        .arg(format!("root@{ip}"))
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .spawn()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    Scanning,
    Connected { ip: String },
}

pub struct Tunnel {
    status: Mutex<TunnelStatus>,
    current_ssh_pid: AtomicU32, // 0 = none
    restart: Notify,
    stop: Notify,
}

impl Tunnel {
    pub fn new() -> Arc<Self> {
        Arc::new(Tunnel {
            status: Mutex::new(TunnelStatus::Scanning),
            current_ssh_pid: AtomicU32::new(0),
            restart: Notify::new(),
            stop: Notify::new(),
        })
    }

    pub fn status(&self) -> TunnelStatus {
        self.status.lock().unwrap().clone()
    }

    fn kill_current(&self) {
        let pid = self.current_ssh_pid.swap(0, Ordering::SeqCst);
        if pid != 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
    }

    /// Forces an immediate reconnect instead of waiting out the current link.
    pub fn restart_now(&self) {
        self.kill_current();
        self.restart.notify_one();
    }

    /// Tears the tunnel down for good — call once, right before app exit.
    pub fn quit(&self) {
        self.kill_current();
        self.stop.notify_one();
    }

    pub fn spawn_supervisor(self: &Arc<Self>, port: u16) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                *this.status.lock().unwrap() = TunnelStatus::Scanning;
                let device = tokio::select! {
                    d = find_device() => d,
                    _ = this.stop.notified() => return,
                };
                let Some(ip) = device else {
                    tokio::select! {
                        _ = tokio::time::sleep(RESCAN_DELAY) => continue,
                        _ = this.stop.notified() => return,
                    }
                };

                *this.status.lock().unwrap() = TunnelStatus::Connected { ip: ip.clone() };
                if let Ok(mut child) = spawn_reverse_tunnel(&ip, port).await {
                    if let Some(pid) = child.id() {
                        this.current_ssh_pid.store(pid, Ordering::SeqCst);
                    }
                    tokio::select! {
                        _ = child.wait() => {}
                        _ = this.restart.notified() => { let _ = child.kill().await; }
                        _ = this.stop.notified() => { let _ = child.kill().await; return; }
                    }
                    this.current_ssh_pid.store(0, Ordering::SeqCst);
                }

                tokio::select! {
                    _ = tokio::time::sleep(RESCAN_DELAY) => {}
                    _ = this.stop.notified() => return,
                }
            }
        });
    }
}
