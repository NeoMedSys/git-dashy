//! Other gitdashys on the LAN: who is running, and whether their auto is on.
//!
//! Every 2s each app broadcasts `{"id": .., "auto": ..}` on UDP 50000 and listens for the others. A
//! peer not heard from for 5s is gone. The ☰ menu's LAN row turns both off and on while running;
//! `PRS_LAN=0` starts it off. `PRS_LAN=10.20.0.0/16,192.168.5.0/24`
//! keeps both to those networks: it announces to each network's own broadcast address, never
//! 255.255.255.255, and hears only packets from inside them. A laptop on cafe wifi sends nothing onto it
//! and takes nothing from it, VPN or not. Name the LAN's real subnet: a /16 on a /24 LAN announces to a
//! broadcast address the LAN does not answer to.
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

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
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
    // a zero nonce only makes the scrambling repeat, which obfuscation survives; said, not hidden
    if let Err(e) = getrandom::fill(&mut nonce) {
        log::debug!("lan: no randomness for a nonce: {e}");
    }
    [&nonce[..], &stream(&nonce, plain)].concat()
}

fn open(packet: &[u8]) -> Vec<u8> {
    if packet.len() < 8 {
        return Vec::new();
    }
    stream(&packet[..8], &packet[8..])
}

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

/// Whether a packet from `ip` counts: every sender with no networks named, else only those inside one.
fn inside(nets: &[(u32, u32)], ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => nets.is_empty() || nets.iter().any(|(net, mask)| u32::from(ip) & mask == *net),
        IpAddr::V6(_) => false,
    }
}

/// Where an announce goes: each named network's own broadcast address, or everywhere with none named.
fn targets(nets: &[(u32, u32)]) -> Vec<Ipv4Addr> {
    if nets.is_empty() {
        return vec![Ipv4Addr::BROADCAST];
    }
    nets.iter()
        .map(|(net, mask)| Ipv4Addr::from(net | !mask))
        .collect()
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
    if !crate::config::lan() {
        return (Vec::new(), 0);
    }
    peers_at(Instant::now())
}

fn peers_at(at: Instant) -> (Vec<Value>, usize) {
    let mut peers = PEERS.lock().unwrap_or_else(|e| e.into_inner());
    prune(&mut peers, at);
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
    // unset, "0" or "1": everywhere, which is an empty list. "0" is off, and that is config.lan: the
    // threads still start so the menu can turn it on without a restart.
    let Some(nets) = nets(if limit == "0" || limit == "1" { "" } else { &limit }) else {
        log::warn!("lan: PRS_LAN={limit} is not 0, 1 or a list of networks; LAN presence is off");
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
    let to = targets(&nets);
    std::thread::spawn(move || {
        let Ok(s) = UdpSocket::bind("0.0.0.0:0") else {
            log::debug!("lan: no socket to announce on");
            return;
        };
        let _ = s.set_broadcast(true);
        loop {
            if !crate::config::lan() {
                std::thread::sleep(EVERY);
                continue;
            }
            let packet = seal(
                json!({"id": id, "auto": state.lock().auto})
                    .to_string()
                    .as_bytes(),
            );
            for ip in &to {
                // off that network the address routes nowhere useful and the send fails or is dropped
                if let Err(e) = s.send_to(&packet, (*ip, PORT)) {
                    log::debug!("lan: announce to {ip} failed: {e}");
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
                // switched off in the menu, or from outside the named networks: not a peer
                Ok((_, from)) if !crate::config::lan() || !inside(&nets, from.ip()) => {}
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
        let base = Instant::now();
        let now = base + Duration::from_secs(10);
        *PEERS.lock().unwrap() = vec![
            ("a1".into(), true, now),
            ("b2".into(), false, now),
            ("gone".into(), true, base),
        ];
        let (list, auto) = peers_at(now);
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
        assert!(!inside(&n, "::1".parse().unwrap()));
        assert!(
            inside(&[], "8.8.8.8".parse().unwrap()),
            "no networks named hears everyone"
        );
        assert_eq!(
            targets(&n),
            [Ipv4Addr::new(10, 20, 255, 255), Ipv4Addr::new(192, 168, 5, 7)],
            "each network's own broadcast, never the global one"
        );
        assert_eq!(targets(&[]), [Ipv4Addr::BROADCAST]);
        assert_eq!(nets("office"), None);
        assert_eq!(nets("10.0.0.0/33"), None);
        assert_eq!(
            nets("10.0.0.0/8,oops"),
            None,
            "one bad entry is not a partial list"
        );
    }
}
