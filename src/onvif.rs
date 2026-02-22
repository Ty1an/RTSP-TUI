use anyhow::Result;
use quick_xml::Reader;
use quick_xml::events::Event;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;

const WS_DISCOVERY_ADDR: &str = "239.255.255.250:3702";

pub async fn discover_hosts(timeout: Duration) -> Result<Vec<Ipv4Addr>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    let probe = build_probe_message();
    // Send a few probe packets to reduce packet loss impact on noisy networks.
    for _ in 0..3 {
        let _ = socket.send_to(probe.as_bytes(), WS_DISCOVERY_ADDR).await;
    }

    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0_u8; 64 * 1024];
    let mut hosts = HashSet::new();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline.saturating_duration_since(now);
        let recv = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
        let Ok(Ok((size, remote_addr))) = recv else {
            break;
        };

        let payload = String::from_utf8_lossy(&buf[..size]);
        let mut saw_xaddr_ip = false;

        for xaddr in extract_xaddrs(&payload) {
            if let Ok(url) = url::Url::parse(&xaddr)
                && let Some(host) = url.host_str()
                && let Ok(ip) = host.parse::<IpAddr>()
                && let IpAddr::V4(v4) = ip
            {
                hosts.insert(v4);
                saw_xaddr_ip = true;
            }
        }

        if !saw_xaddr_ip && let IpAddr::V4(v4) = remote_addr.ip() {
            hosts.insert(v4);
        }
    }

    let mut discovered = hosts.into_iter().collect::<Vec<_>>();
    discovered.sort_unstable();
    Ok(discovered)
}

fn build_probe_message() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = u128::from(std::process::id());
    let message_id = format!("uuid:{ts:032x}{pid:08x}");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
            xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
            xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
            xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
  <e:Header>
    <w:MessageID>{message_id}</w:MessageID>
    <w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
    <w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
  </e:Header>
  <e:Body>
    <d:Probe>
      <d:Types>dn:NetworkVideoTransmitter</d:Types>
    </d:Probe>
  </e:Body>
</e:Envelope>"#
    )
}

fn extract_xaddrs(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut in_xaddrs = false;
    let mut output = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(start)) => {
                in_xaddrs = local_name(start.name().as_ref()) == b"XAddrs";
            }
            Ok(Event::End(_)) => {
                in_xaddrs = false;
            }
            Ok(Event::Text(text)) if in_xaddrs => {
                if let Ok(decoded) = text.decode() {
                    output.extend(
                        decoded
                            .split_whitespace()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(std::borrow::ToOwned::to_owned),
                    );
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) | Err(_) => {}
        }

        buf.clear();
    }

    output
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}
