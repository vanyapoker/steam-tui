//! Steam Datagram Relay data-center pings. The relay network layout is public
//! (`ISteamApps/GetSDRConfig`); round-trip times are measured with ICMP echo
//! over unprivileged datagram sockets. The numbers feed the game's matchmaking
//! ping report; no relay connection is made here.
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

const CONFIG_URL: &str = "https://api.steampowered.com/ISteamApps/GetSDRConfig/v1/?appid=";
const ATTEMPTS: usize = 3;

/// Public routing configuration, bounded before crossing the game IPC boundary.
pub(super) fn parse_config(config: &serde_json::Value) -> Option<steam_ipc::SdrConfig> {
    fn pop_id(code: &str) -> Option<u32> {
        if !(3..=4).contains(&code.len())
            || !code
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return None;
        }
        let b = code.as_bytes();
        Some(
            (u32::from(b[0]) << 16)
                | (u32::from(b[1]) << 8)
                | u32::from(b[2])
                | (u32::from(b.get(3).copied().unwrap_or(0)) << 24),
        )
    }
    let revision = u32::try_from(config["revision"].as_u64()?).ok()?;
    let key = config["relay_public_key"].as_str()?;
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut pops = Vec::new();
    for (code, value) in config["pops"].as_object()?.iter().take(256) {
        let Some(id) = pop_id(code) else { continue };
        let relays = value["relays"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .filter_map(|r| {
                let ip: Ipv4Addr = r["ipv4"].as_str()?.parse().ok()?;
                let start = u16::try_from(r["port_range"][0].as_u64()?).ok()?;
                let end = u16::try_from(r["port_range"][1].as_u64()?).ok()?;
                if ip.is_unspecified()
                    || ip.is_loopback()
                    || ip.is_multicast()
                    || ip.is_private()
                    || start == 0
                    || end < start
                {
                    return None;
                }
                Some(steam_ipc::SdrRelay {
                    ipv4: u32::from(ip),
                    port_start: start,
                    port_end: end,
                })
            })
            .collect();
        pops.push(steam_ipc::SdrPop { id, relays });
    }
    let typical_pings = config["typical_pings"]
        .as_array()
        .into_iter()
        .flatten()
        .take(65536)
        .filter_map(|row| {
            Some((
                pop_id(row[0].as_str()?)?,
                pop_id(row[1].as_str()?)?,
                u32::try_from(row[2].as_u64()?).ok()?,
            ))
        })
        .collect();
    Some(steam_ipc::SdrConfig {
        revision,
        relay_public_key: key.into(),
        pops,
        typical_pings,
    })
}

/// One echo round trip, or `None` on timeout/permission failure.
fn ping_once(ip: Ipv4Addr, sequence: u16) -> Option<Duration> {
    // SAFETY: plain socket calls on a descriptor owned for the duration of the call.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP);
        if fd < 0 {
            return None;
        }
        let timeout = libc::timeval {
            tv_sec: 1,
            tv_usec: 0,
        };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw const timeout).cast(),
            std::mem::size_of::<libc::timeval>() as u32,
        );
        // ICMP echo request: type 8, code 0, checksum (kernel fills it), id (kernel
        // assigns), sequence.
        let mut packet = [0u8; 16];
        packet[0] = 8;
        packet[6..8].copy_from_slice(&sequence.to_be_bytes());
        let mut addr: libc::sockaddr_in = std::mem::zeroed();
        addr.sin_family = libc::AF_INET as u16;
        addr.sin_addr.s_addr = u32::from_ne_bytes(ip.octets());
        let started = Instant::now();
        let sent = libc::sendto(
            fd,
            packet.as_ptr().cast(),
            packet.len(),
            0,
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as u32,
        );
        let mut reply = [0u8; 64];
        let received = if sent > 0 {
            libc::recv(fd, reply.as_mut_ptr().cast(), reply.len(), 0)
        } else {
            -1
        };
        libc::close(fd);
        // Type 0 is an echo reply; anything else is an error report.
        (received >= 8 && reply[0] == 0).then(|| started.elapsed())
    }
}

fn best_ping(ips: &[Ipv4Addr]) -> i32 {
    let mut best: Option<Duration> = None;
    for (index, ip) in ips.iter().take(2).enumerate() {
        for attempt in 0..ATTEMPTS {
            if let Some(rtt) = ping_once(*ip, (index * ATTEMPTS + attempt) as u16) {
                best = Some(best.map_or(rtt, |b| b.min(rtt)));
            }
        }
    }
    best.map_or(-1, |rtt| {
        (rtt.as_secs_f64() * 1000.0).ceil().max(1.0) as i32
    })
}

/// `(pop code, ms or -1)` for every data center in the app's SDR config.
pub async fn measure(app_id: u32) -> Result<Vec<(String, i32)>, String> {
    let body = reqwest::get(format!("{CONFIG_URL}{app_id}"))
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("SDR config unavailable: {e}"))?
        .text()
        .await
        .map_err(|e| format!("SDR config unreadable: {e}"))?;
    let config: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("SDR config malformed: {e}"))?;
    let pops = config
        .get("pops")
        .and_then(|p| p.as_object())
        .ok_or("SDR config has no pops")?;
    let mut targets = Vec::new();
    for (code, pop) in pops {
        if code.len() < 3 || code.len() > 4 || !code.is_ascii() {
            continue;
        }
        let ips: Vec<Ipv4Addr> = pop
            .get("relays")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
            .filter_map(|relay| relay.get("ipv4")?.as_str()?.parse().ok())
            .collect();
        if !ips.is_empty() {
            targets.push((code.clone(), ips));
        }
    }
    let mut tasks = Vec::new();
    for (code, ips) in targets {
        tasks.push(tokio::task::spawn_blocking(move || (code, best_ping(&ips))));
    }
    let mut out = Vec::new();
    for task in tasks {
        if let Ok(entry) = task.await {
            out.push(entry);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_config_preserves_relay_endpoints_and_four_letter_pop_ids() {
        // Synthetic data (documentation address range, made-up key).
        let config = serde_json::json!({
            "revision": 1_700_000_000u64,
            "relay_public_key": "ab".repeat(32),
            "pops": {
                "sto": {"relays": [{"ipv4": "198.51.100.41", "port_range": [27015, 27060]}]},
                "sto2": {"relays": [{"ipv4": "198.51.100.61", "port_range": [27015, 27060]}]}
            },
            "typical_pings": [["sto", "sto2", 7]]
        });
        let parsed = parse_config(&config).unwrap();
        assert_eq!(parsed.revision, 1_700_000_000);
        assert_eq!(parsed.relay_public_key.len(), 64);
        let sto = parsed.pops.iter().find(|p| p.id == 0x73746f).unwrap();
        assert_eq!(
            sto.relays[0].ipv4,
            u32::from(Ipv4Addr::new(198, 51, 100, 41))
        );
        assert_eq!(
            (sto.relays[0].port_start, sto.relays[0].port_end),
            (27015, 27060)
        );
        assert!(parsed.pops.iter().any(|p| p.id == 0x3273746f));
        assert!(!parsed.typical_pings.is_empty());
    }

    #[test]
    fn routing_config_rejects_invalid_keys_and_filters_bad_endpoints() {
        let mut config = serde_json::json!({"revision": 1, "relay_public_key": "ab".repeat(32),
            "pops": {"ams": {"relays": [
                {"ipv4": "127.0.0.1", "port_range": [27015,27060]},
                {"ipv4": "198.51.100.36", "port_range": [27060,27015]},
                {"ipv4": "198.51.100.36", "port_range": [27015,70000]},
                {"ipv4": "198.51.100.36", "port_range": [27015,27060]}]}}});
        let parsed = parse_config(&config).unwrap();
        assert_eq!(parsed.pops.len(), 1);
        assert_eq!(parsed.pops[0].relays.len(), 1);
        config["relay_public_key"] = "gg".repeat(32).into();
        assert!(parse_config(&config).is_none());
    }
}
