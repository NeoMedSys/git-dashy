//! Other gitdashys on the LAN: who is running, and whether their auto is on.
//!
//! Every 2s each app broadcasts `{"id": .., "auto": ..}` on UDP 50000 and listens for the others. A
//! peer not heard from for 5s is gone. `PRS_LAN=0` turns both off. `PRS_LAN=10.20.0.0/16,192.168.5.0/24`
//! keeps both to those networks: every tick checks this machine's own address, so a laptop that leaves
//! the office goes quiet by itself and comes back when it returns.
//!
//! ponytail: the id is random per launch, NOT your login or a hash of it. A hash of a login is
//! reversed by hashing a list of logins, and a stable one follows you from network to network; a
//! fresh random id says nothing and links nothing. The price: the list says how many and whether
//! auto is on, never who.
//!
//! ponytail: packets are scrambled with a key built into the app, so on the wire they are noise to
//! anyone without gitdashy. The repo is public, so this is obfuscation, not security: anyone who reads
//! this file can read them. A per-team key from the team checkout is the upgrade if it must be private.
//!
//! ponytail: std cannot set SO_REUSEADDR before bind, so a second gitdashy on the same machine fails
//! the bind and shows no peers; it still announces. socket2 fixes it if that ever matters.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::state::State;

const PORT: u16 = 50000;
const EVERY: Duration = Duration::from_secs(2);
const GONE: Duration = Duration::from_secs(5);
/// Anyone on the LAN can send packets, so a flood of made-up ids stops here instead of growing the list.
const MAX: usize = 64;

const KEY: &[u8] = b"gitdashy-lan-v1";

/// XOR with a SHA-256 keystream over (key, nonce, block). The same call scrambles and unscrambles.
fn stream(nonce: &[u8], data: &[u8]) -> Vec<u8> {
    data.chunks(32)
        .enumerate()
        .flat_map(|(i, chunk)| {
            let pad = Sha256::new()
                .chain_update(KEY)
                .chain_update(nonce)
                .chain_update((i as u32).to_le_bytes())
                .finalize();
            chunk.iter().zip(pad).map(|(b, p)| b ^ p).collect::<Vec<_>>()
        })
        .collect()
}

/// nonce(8) + scrambled json. A fresh nonce each time, so the same state never looks the same twice.
fn seal(plain: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 8];
    let _ = getrandom::fill(&mut nonce);
    [&nonce[..], &stream(&nonce, plain)].concat()
}

fn open(packet: &[u8]) -> Vec<u8> {
    if packet.len() < 8 {
        return Vec::new();
    }
    stream(&packet[..8], &packet[8..])
}

/// Whether this machine is on a network PRS_LAN allows, as of the sender's last tick.
static ON: AtomicBool = AtomicBool::new(false);

/// `PRS_LAN` as networks: "a.b.c.d/n" or a bare address, comma separated, as (network, mask).
/// None when any entry does not parse, so a typo keeps LAN off rather than on everywhere.
fn nets(v: &str) -> Option<Vec<(u32, u32)>> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let (ip, bits) = s.split_once('/').unwrap_or((s, "32"));
            let ip = u32::from(ip.parse::<Ipv4Addr>().ok()?);
            let bits: u32 = bits.parse().ok().filter(|b| *b <= 32)?;
            let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
            Some((ip & mask, mask))
        })
        .collect()
}

fn inside(nets: &[(u32, u32)], ip: Ipv4Addr) -> bool {
    nets.iter().any(|(net, mask)| u32::from(ip) & mask == *net)
}

/// This machine's address on the route out. ponytail: connect() on UDP only picks a route, it sends
/// nothing, so no packet leaves and nothing needs to answer.
fn local_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect(("10.255.255.255", 1)).ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        std::net::IpAddr::V6(_) => None,
    }
}

/// id -> (auto, last heard).
static PEERS: Mutex<Vec<(String, bool, Instant)>> = Mutex::new(Vec::new());

/// Drop whoever has gone quiet.
fn prune(peers: &mut Vec<(String, bool, Instant)>, at: Instant) {
    peers.retain(|p| at.duration_since(p.2) < GONE);
}

/// Take one packet into the list. Junk and our own echo are ignored.
fn absorb(peers: &mut Vec<(String, bool, Instant)>, me: &str, packet: &[u8], at: Instant) {
    // before the cap, or a list full of peers that have gone quiet turns a live one away
    prune(peers, at);
    let v: Value = serde_json::from_slice(&open(packet)).unwrap_or_default();
    if let (Some(id), Some(auto)) = (v["id"].as_str(), v["auto"].as_bool()) {
        if id != me && id.len() <= 16 {
            peers.retain(|p| p.0 != id);
            if peers.len() < MAX {
                peers.push((id.to_string(), auto, at));
            }
        }
    }
}

/// The peers still live, and how many of them run auto, for the payload.
pub fn peers() -> (Vec<Value>, usize) {
    let mut peers = PEERS.lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut peers, Instant::now());
    let auto = peers.iter().filter(|p| p.1).count();
    (
        peers
            .iter()
            .map(|(id, auto, _)| json!({"id": id, "auto": auto}))
            .collect(),
        auto,
    )
}

pub fn start(state: State) {
    let limit = std::env::var("PRS_LAN").unwrap_or_default();
    if limit == "0" {
        return;
    }
    // unset or "1": everywhere, which is an empty list
    let Some(nets) = nets(if limit == "1" { "" } else { &limit }) else {
        log::debug!("lan: PRS_LAN={limit} is not a list of networks, not starting");
        return;
    };
    // 4 bytes: a peer that drew our id would be dropped as our own echo
    let mut raw = [0u8; 4];
    // an all-zero id on every failing machine would read each other's packets as their own echo
    if let Err(e) = getrandom::fill(&mut raw) {
        log::debug!("lan: no randomness for an id, not starting: {e}");
        return;
    }
    let me: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let id = me.clone();
    std::thread::spawn(move || {
        let Ok(s) = UdpSocket::bind("0.0.0.0:0") else {
            log::debug!("lan: no socket to announce on");
            return;
        };
        let _ = s.set_broadcast(true);
        loop {
            let on = nets.is_empty() || local_ip().is_some_and(|ip| inside(&nets, ip));
            ON.store(on, Ordering::Relaxed);
            if on {
                let packet = json!({"id": id, "auto": state.lock().auto}).to_string();
                if let Err(e) = s.send_to(&seal(packet.as_bytes()), ("255.255.255.255", PORT)) {
                    log::debug!("lan: announce failed: {e}");
                }
            }
            std::thread::sleep(EVERY);
        }
    });
    std::thread::spawn(move || {
        let Ok(s) = UdpSocket::bind(("0.0.0.0", PORT)) else {
            log::debug!("lan: port {PORT} taken, not listening");
            return;
        };
        let mut buf = [0u8; 256];
        loop {
            match s.recv_from(&mut buf) {
                // off this network: heard nothing, and whoever was listed ages out in GONE
                Ok(_) if !ON.load(Ordering::Relaxed) => {}
                Ok((n, _)) => {
                    let mut peers = PEERS.lock().unwrap_or_else(|e| e.into_inner());
                    absorb(&mut peers, &me, &buf[..n], Instant::now());
                }
                // an error that repeats (interface down, a Windows ConnectionReset) would spin a core
                Err(e) => {
                    log::debug!("lan: recv failed: {e}");
                    std::thread::sleep(EVERY);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(json: &str) -> Vec<u8> {
        seal(json.as_bytes())
    }

    #[test]
    fn scrambled_on_the_wire_and_readable_back() {
        let plain: &[u8] = br#"{"id":"a1","auto":true}"#;
        let (a, b) = (seal(plain), seal(plain));
        assert_ne!(a, b, "a fresh nonce each time");
        assert!(!a.windows(4).any(|w| w == b"auto"), "no plaintext on the wire");
        assert_eq!(open(&a), plain.to_vec());
        let mut p = Vec::new();
        absorb(&mut p, "me", plain, Instant::now());
        assert!(p.is_empty(), "an unscrambled packet is not ours");
    }

    #[test]
    fn absorbs_peers_and_drops_junk_self_and_stale() {
        let t0 = Instant::now();
        let mut p = Vec::new();
        absorb(&mut p, "me", &pk(r#"{"id":"a1","auto":true}"#), t0);
        absorb(&mut p, "me", &pk(r#"{"id":"me","auto":true}"#), t0);
        absorb(&mut p, "me", &pk("not json"), t0);
        absorb(&mut p, "me", &pk(r#"{"id":"a1","auto":false}"#), t0);
        assert_eq!(p.len(), 1);
        assert!(!p[0].1, "the newest packet wins");
        absorb(
            &mut p,
            "me",
            &pk(r#"{"id":"b2","auto":true}"#),
            t0 + Duration::from_secs(4),
        );
        prune(&mut p, t0 + Duration::from_secs(6));
        assert_eq!(
            p.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["b2"],
            "a1 went quiet"
        );
        for i in 0..1000 {
            absorb(
                &mut p,
                "me",
                &pk(&format!(r#"{{"id":"f{i}","auto":true}}"#)),
                t0 + Duration::from_secs(6),
            );
        }
        assert_eq!(p.len(), MAX, "a flood of ids is capped");
        absorb(
            &mut p,
            "me",
            &pk(r#"{"id":"live","auto":false}"#),
            t0 + Duration::from_secs(12),
        );
        assert_eq!(
            p.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
            ["live"],
            "a full list gone quiet takes a live peer"
        );
    }

    #[test]
    fn peers_lists_the_live_ones_and_counts_auto() {
        let now = Instant::now();
        // a clock under 10s old has no instant that far back
        let Some(old) = now.checked_sub(Duration::from_secs(10)) else {
            return;
        };
        *PEERS.lock().unwrap() = vec![
            ("a1".into(), true, now),
            ("b2".into(), false, now),
            ("gone".into(), true, old),
        ];
        let (list, auto) = peers();
        assert_eq!(
            list,
            [
                json!({"id": "a1", "auto": true}),
                json!({"id": "b2", "auto": false})
            ]
        );
        assert_eq!(auto, 1, "the quiet one is neither listed nor counted");
    }

    #[test]
    fn prs_lan_names_networks_and_a_typo_is_not_everywhere() {
        let n = nets("10.20.0.0/16, 192.168.5.7").unwrap();
        assert!(inside(&n, "10.20.3.4".parse().unwrap()));
        assert!(
            inside(&n, "192.168.5.7".parse().unwrap()),
            "a bare address is a /32"
        );
        assert!(!inside(&n, "192.168.5.8".parse().unwrap()));
        assert!(
            !inside(&n, "10.21.0.1".parse().unwrap()),
            "the cafe is not the office"
        );
        assert!(inside(&nets("0.0.0.0/0").unwrap(), "8.8.8.8".parse().unwrap()));
        assert_eq!(nets(""), Some(vec![]), "unset is everywhere");
        assert_eq!(nets("office"), None);
        assert_eq!(nets("10.0.0.0/33"), None);
        assert_eq!(
            nets("10.0.0.0/8,oops"),
            None,
            "one bad entry is not a partial list"
        );
    }
}
