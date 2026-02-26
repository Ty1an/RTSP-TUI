use super::DiscoveredItem;
use crate::cache::{DiscoveredStream, SelectedStream};
use anyhow::{Context, Result, anyhow};
use url::Url;

pub(crate) fn sanitize_discovered(streams: Vec<DiscoveredStream>) -> Vec<DiscoveredItem> {
    let mut seen = std::collections::HashSet::new();
    let mut sanitized = Vec::new();
    let mut hosts_without_stream_endpoint: std::collections::HashMap<String, u16> =
        std::collections::HashMap::new();
    let mut hosts_with_stream_endpoint = std::collections::HashSet::new();

    for stream in streams {
        let Ok(base_url) = normalize_rtsp_url_without_auth(&stream.url) else {
            continue;
        };
        if !seen.insert(base_url.clone()) {
            continue;
        }
        let Ok(parsed) = url::Url::parse(&base_url) else {
            continue;
        };
        let host = parsed.host_str().unwrap_or("unknown");
        let port = parsed.port_or_known_default().unwrap_or(554);
        let host_port = format!("{host}:{port}");

        if !is_selectable_stream_endpoint(&base_url) {
            hosts_without_stream_endpoint
                .entry(host_port)
                .and_modify(|status| *status = (*status).max(stream.status))
                .or_insert(stream.status);
            continue;
        }
        hosts_with_stream_endpoint.insert(host_port);

        sanitized.push(DiscoveredItem {
            label: format!("{host}:{port} {}", endpoint_display(&base_url)),
            base_url,
            status: stream.status,
        });
    }

    for (host_port, status) in hosts_without_stream_endpoint {
        if hosts_with_stream_endpoint.contains(&host_port) {
            continue;
        }
        for endpoint in ["/stream2", "/stream1"] {
            let base_url = format!("rtsp://{host_port}{endpoint}");
            sanitized.push(DiscoveredItem {
                label: format!("{host_port} {endpoint}"),
                base_url,
                status,
            });
        }
    }

    sanitized.sort_by(|a, b| a.label.cmp(&b.label));
    sanitized
}

pub(crate) fn is_selectable_stream_endpoint(url: &str) -> bool {
    let hint = endpoint_hint(url);
    // Keep explicit stream endpoints, exclude generic vendor-specific "streaming/channels" style.
    hint.contains("/stream1")
        || hint.contains("/stream2")
        || hint.ends_with("/stream")
        || hint.contains("/stream?")
}

pub(crate) fn endpoint_hint(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "/".to_owned();
    };
    let mut hint = parsed.path().to_ascii_lowercase();
    if let Some(query) = parsed.query()
        && !query.is_empty()
    {
        hint.push('?');
        hint.push_str(&query.to_ascii_lowercase());
    }
    if hint.is_empty() {
        "/".to_owned()
    } else {
        hint
    }
}

pub(crate) fn endpoint_display(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "/".to_owned();
    };
    let mut path = parsed.path().to_owned();
    if let Some(query) = parsed.query()
        && !query.is_empty()
    {
        path.push('?');
        path.push_str(query);
    }
    if path.is_empty() {
        "/".to_owned()
    } else {
        path
    }
}

pub(crate) fn camera_ip_key(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(host.to_owned())
}

pub(crate) fn camera_auth_key(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default().unwrap_or(554);
    Some(format!("{host}:{port}"))
}

pub(crate) fn inherited_camera_profile(
    streams: &[SelectedStream],
    base_url: &str,
) -> Option<SelectedStream> {
    let target_key = camera_auth_key(base_url)?;
    streams
        .iter()
        .find(|candidate| {
            if camera_auth_key(&candidate.base_url).as_deref() != Some(target_key.as_str()) {
                return false;
            }
            candidate
                .username
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
                || candidate
                    .password
                    .as_deref()
                    .is_some_and(|pass| !pass.is_empty())
                || candidate
                    .display_name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
        })
        .cloned()
}

pub(crate) fn dedupe_selected_streams_by_url(streams: Vec<SelectedStream>) -> Vec<SelectedStream> {
    let mut deduped: Vec<SelectedStream> = Vec::new();
    for stream in streams {
        if let Some(existing) = deduped
            .iter_mut()
            .find(|item| item.base_url == stream.base_url)
        {
            // Keep the latest endpoint choice for this exact stream URL, preserving non-empty values.
            existing.base_url = stream.base_url.clone();
            existing.enabled = stream.enabled;
            if stream
                .display_name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
            {
                existing.display_name = stream.display_name.clone();
            }
            if stream.username.is_some() {
                existing.username = stream.username.clone();
            }
            if stream.password.is_some() {
                existing.password = stream.password.clone();
            }
            continue;
        }
        deduped.push(stream);
    }
    deduped
}

pub(crate) fn dedupe_discovered_streams_by_url(
    streams: Vec<DiscoveredStream>,
) -> Vec<DiscoveredStream> {
    let mut by_url: std::collections::HashMap<String, DiscoveredStream> =
        std::collections::HashMap::new();
    for stream in streams {
        let Ok(normalized_url) = normalize_rtsp_url_without_auth(&stream.url) else {
            continue;
        };
        let normalized = DiscoveredStream {
            url: normalized_url.clone(),
            ..stream
        };
        match by_url.entry(normalized_url) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(normalized);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let current = entry.get();
                if normalized.discovered_at_unix >= current.discovered_at_unix {
                    entry.insert(normalized);
                }
            }
        }
    }

    let mut deduped = by_url.into_values().collect::<Vec<_>>();
    deduped.sort_by(|a, b| a.url.cmp(&b.url));
    deduped
}

pub(crate) fn normalize_rtsp_url_without_auth(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty stream URL"));
    }

    let normalized = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("rtsp://{trimmed}")
    };

    let mut parsed =
        url::Url::parse(&normalized).with_context(|| format!("invalid RTSP URL '{trimmed}'"))?;
    if parsed.scheme() != "rtsp" {
        return Err(anyhow!("unsupported scheme '{}'", parsed.scheme()));
    }

    parsed
        .set_username("")
        .map_err(|()| anyhow!("failed clearing username"))?;
    parsed
        .set_password(None)
        .map_err(|()| anyhow!("failed clearing password"))?;

    Ok(parsed.to_string())
}

pub(crate) fn apply_rtsp_credentials(url: &str, username: &str, password: &str) -> Result<String> {
    let mut parsed = url::Url::parse(url).with_context(|| format!("invalid RTSP URL '{url}'"))?;
    if username.is_empty() {
        return Ok(parsed.to_string());
    }

    // Prefer URL crate's native userinfo handling first.
    if parsed.set_username(username).is_ok() {
        if password.is_empty() {
            parsed
                .set_password(None)
                .map_err(|()| anyhow!("failed clearing password"))?;
        } else {
            parsed
                .set_password(Some(password))
                .map_err(|()| anyhow!("failed applying password"))?;
        }
        return Ok(parsed.to_string());
    }

    // Fallback for user/pass containing characters rejected by set_username/set_password.
    let encoded_user = percent_encode_userinfo(username);
    let encoded_pass = percent_encode_userinfo(password);
    let scheme = parsed.scheme();
    let tail = &parsed[url::Position::BeforeHost..];

    if password.is_empty() {
        Ok(format!("{scheme}://{encoded_user}@{tail}"))
    } else {
        Ok(format!("{scheme}://{encoded_user}:{encoded_pass}@{tail}"))
    }
}

pub(crate) fn percent_decode_userinfo(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;

    while idx < bytes.len() {
        if bytes[idx] == b'%'
            && idx + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[idx + 1]), hex_value(bytes[idx + 2]))
        {
            decoded.push((hi << 4) | lo);
            idx += 3;
            continue;
        }

        decoded.push(bytes[idx]);
        idx += 1;
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn percent_encode_userinfo(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for b in value.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if is_unreserved {
            encoded.push(char::from(b));
        } else {
            let _ = std::fmt::Write::write_fmt(&mut encoded, format_args!("%{:02X}", b));
        }
    }
    encoded
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
