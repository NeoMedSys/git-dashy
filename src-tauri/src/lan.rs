//! Other gitdashys on the LAN: who is running, and whether their auto is on.
//!
//! Every 2s each app broadcasts `{"id": .., "auto": ..}` on UDP 50000 and listens for the others. A
//! peer not heard from for 5s is gone. `PRS_LAN=0` turns both off.
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

use std::net::UdpSocket;
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
    if std::env::var("PRS_LAN").is_ok_and(|v| v == "0") {
        return;
    }
    // 4 bytes: a peer that drew our id would be dropped as our own echo
    let mut raw = [0u8; 4];
    let _ = getrandom::fill(&mut raw);
    let me: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let id = me.clone();
    std::thread::spawn(move || {
        let Ok(s) = UdpSocket::bind("0.0.0.0:0") else {
            log::debug!("lan: no socket to announce on");
            return;
        };
        let _ = s.set_broadcast(true);
        loop {
            let packet = json!({"id": id, "auto": state.lock().auto}).to_string();
            if let Err(e) = s.send_to(&seal(packet.as_bytes()), ("255.255.255.255", PORT)) {
                log::debug!("lan: announce failed: {e}");
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
        *PEERS.lock().unwrap() = vec![
            ("a1".into(), true, now),
            ("b2".into(), false, now),
            ("gone".into(), true, now - Duration::from_secs(10)),
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
}
