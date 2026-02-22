use crate::cache::{DiscoveredStream, now_unix};
use crate::cli::DiscoverArgs;
use crate::onvif;
use anyhow::{Context, Result, anyhow};
use if_addrs::{IfAddr, get_if_addrs};
use ipnet::Ipv4Net;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc};

const STATUS_OK: u16 = 200;
const STATUS_UNAUTHORIZED: u16 = 401;
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub async fn discover_streams(args: &DiscoverArgs) -> Result<Vec<DiscoveredStream>> {
    discover_streams_with_events(args, None).await
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoveryProgress {
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    Progress(DiscoveryProgress),
    StreamFound(DiscoveredStream),
}

pub async fn discover_streams_with_events(
    args: &DiscoverArgs,
    event_tx: Option<mpsc::UnboundedSender<DiscoveryEvent>>,
) -> Result<Vec<DiscoveredStream>> {
    let cidrs = resolve_target_cidrs(args)?;
    let onvif_hosts = if args.onvif {
        match onvif::discover_hosts(Duration::from_millis(args.timeout_ms.max(200))).await {
            Ok(hosts) => hosts,
            Err(err) => {
                eprintln!("Warning: ONVIF host discovery failed: {err:#}");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    if cidrs.is_empty() && onvif_hosts.is_empty() {
        return Err(anyhow!(
            "no IPv4 subnet to scan. Pass --cidr explicitly (example: --cidr 192.168.1.0/24) or enable ONVIF discovery"
        ));
    }

    let paths = normalized_paths(&args.paths);
    let mut target_keys = HashSet::new();
    let mut targets = Vec::new();
    for cidr in &cidrs {
        for host in cidr.hosts().take(args.max_hosts_per_subnet) {
            for port in &args.ports {
                for normalized in &paths {
                    if target_keys.insert((host, *port, normalized.clone())) {
                        targets.push(Target {
                            ip: host,
                            port: *port,
                            path: normalized.clone(),
                        });
                    }
                }
            }
        }
    }
    for host in onvif_hosts {
        for port in &args.ports {
            for normalized in &paths {
                if target_keys.insert((host, *port, normalized.clone())) {
                    targets.push(Target {
                        ip: host,
                        port: *port,
                        path: normalized.clone(),
                    });
                }
            }
        }
    }

    let total_targets = targets.len();
    if let Some(tx) = event_tx.as_ref() {
        let _ = tx.send(DiscoveryEvent::Progress(DiscoveryProgress {
            completed: 0,
            total: total_targets,
        }));
    }

    let semaphore = Arc::new(Semaphore::new(args.concurrency.max(1)));
    let auth = Arc::new(ProbeAuth {
        username: args.username.clone(),
        password: args.password.clone(),
    });
    let timeout = Duration::from_millis(args.timeout_ms.max(50));

    let mut join_set = tokio::task::JoinSet::new();
    for target in targets {
        let semaphore = Arc::clone(&semaphore);
        let auth = Arc::clone(&auth);
        join_set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok()?;
            probe_target(target, timeout, auth.as_ref())
                .await
                .ok()
                .flatten()
        });
    }

    let mut seen = HashSet::new();
    let mut discovered = Vec::new();
    let mut completed = 0_usize;
    while let Some(joined) = join_set.join_next().await {
        let maybe_stream = joined.context("discovery worker task failed")?;
        completed = completed.saturating_add(1);
        if let Some(tx) = event_tx.as_ref() {
            let _ = tx.send(DiscoveryEvent::Progress(DiscoveryProgress {
                completed,
                total: total_targets,
            }));
        }

        if let Some(stream) = maybe_stream {
            if seen.insert(stream.url.clone()) {
                discovered.push(stream.clone());
                if let Some(tx) = event_tx.as_ref() {
                    let _ = tx.send(DiscoveryEvent::StreamFound(stream));
                }
            }
        }
    }

    discovered.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(discovered)
}

fn resolve_target_cidrs(args: &DiscoverArgs) -> Result<Vec<Ipv4Net>> {
    if !args.cidrs.is_empty() {
        let mut parsed = Vec::with_capacity(args.cidrs.len());
        for raw in &args.cidrs {
            let cidr = raw
                .parse::<Ipv4Net>()
                .with_context(|| format!("invalid CIDR '{raw}'"))?
                .trunc();
            parsed.push(cidr);
        }
        return Ok(parsed);
    }

    let interfaces = get_if_addrs().context("failed to enumerate network interfaces")?;
    let mut cidrs = Vec::new();
    let mut seen = HashSet::new();

    for interface in interfaces {
        if interface.is_loopback() {
            continue;
        }

        let IfAddr::V4(v4) = interface.addr else {
            continue;
        };

        // Auto-detected masks can be too narrow (/30-/32) or too broad (/8-/16) for
        // practical camera discovery. Normalize to a local /24 where needed.
        let mut candidates = Vec::with_capacity(2);
        if v4.prefixlen < 24 {
            candidates.push(
                Ipv4Net::new(v4.ip, 24)
                    .context("invalid normalized local /24 CIDR")?
                    .trunc(),
            );
        } else if v4.prefixlen >= 30 {
            candidates.push(
                Ipv4Net::new(v4.ip, v4.prefixlen)
                    .context("invalid local interface CIDR")?
                    .trunc(),
            );
            candidates.push(
                Ipv4Net::new(v4.ip, 24)
                    .context("invalid widened local /24 CIDR")?
                    .trunc(),
            );
        } else {
            candidates.push(
                Ipv4Net::new(v4.ip, v4.prefixlen)
                    .context("invalid local interface CIDR")?
                    .trunc(),
            );
        }

        for net in candidates {
            if seen.insert((net.addr(), net.prefix_len())) {
                cidrs.push(net);
            }
        }
    }

    Ok(cidrs)
}

async fn probe_target(
    target: Target,
    timeout: Duration,
    auth: &ProbeAuth,
) -> Result<Option<DiscoveredStream>> {
    let stream_url = build_rtsp_url(
        target.ip,
        target.port,
        &target.path,
        auth.username.as_deref(),
        auth.password.as_deref(),
    );
    let addr = SocketAddr::from((target.ip, target.port));

    let mut socket = tokio::time::timeout(timeout, TcpStream::connect(addr))
        .await
        .context("connect timeout")?
        .with_context(|| format!("connect failed: {addr}"))?;

    let request = format!(
        "DESCRIBE {stream_url} RTSP/1.0\r\nCSeq: 1\r\nAccept: application/sdp\r\nUser-Agent: {USER_AGENT}\r\n\r\n"
    );

    tokio::time::timeout(timeout, socket.write_all(request.as_bytes()))
        .await
        .context("write timeout")?
        .context("failed writing RTSP request")?;

    let mut response = vec![0_u8; 4096];
    let read_n = tokio::time::timeout(timeout, socket.read(&mut response))
        .await
        .context("read timeout")?
        .context("failed reading RTSP response")?;

    if read_n == 0 {
        return Ok(None);
    }

    let code = parse_rtsp_status(&response[..read_n])?;
    if code != STATUS_OK && code != STATUS_UNAUTHORIZED {
        return Ok(None);
    }

    Ok(Some(DiscoveredStream {
        url: stream_url,
        status: code,
        discovered_at_unix: now_unix(),
    }))
}

fn build_rtsp_url(
    ip: Ipv4Addr,
    port: u16,
    path: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    match (username, password) {
        (Some(user), Some(pass)) => format!("rtsp://{user}:{pass}@{ip}:{port}{path}"),
        (Some(user), None) => format!("rtsp://{user}@{ip}:{port}{path}"),
        _ => format!("rtsp://{ip}:{port}{path}"),
    }
}

fn parse_rtsp_status(raw: &[u8]) -> Result<u16> {
    let line = std::str::from_utf8(raw)
        .context("RTSP response is not valid UTF-8")?
        .lines()
        .next()
        .ok_or_else(|| anyhow!("RTSP response missing status line"))?;

    let mut parts = line.split_whitespace();
    let protocol = parts
        .next()
        .ok_or_else(|| anyhow!("RTSP response missing protocol"))?;
    if !protocol.starts_with("RTSP/") {
        return Err(anyhow!("invalid RTSP protocol marker in response"));
    }

    let code = parts
        .next()
        .ok_or_else(|| anyhow!("RTSP response missing status code"))?
        .parse::<u16>()
        .context("invalid RTSP status code")?;

    Ok(code)
}

fn normalize_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        "/".to_owned()
    } else if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn normalized_paths(raw_paths: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut output = Vec::with_capacity(raw_paths.len());
    for path in raw_paths {
        let normalized = normalize_path(path);
        if seen.insert(normalized.clone()) {
            output.push(normalized);
        }
    }
    output
}

#[derive(Debug, Clone)]
struct Target {
    ip: Ipv4Addr,
    port: u16,
    path: String,
}

#[derive(Debug, Clone, Default)]
struct ProbeAuth {
    username: Option<String>,
    password: Option<String>,
}
