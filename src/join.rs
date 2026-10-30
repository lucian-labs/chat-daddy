//! Join codes: computers that share a code see each other's chats.
//! The code derives a group key. Beacons are sealed with it, and every TCP
//! session runs a fresh-nonce handshake and ChaCha20-Poly1305 frames keyed
//! from it, so a machine without the code can neither read nor be read.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::io::{Read, Write};
use std::net::TcpStream;

type HmacSha256 = Hmac<Sha256>;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ"; // Crockford base32
const CODE_LEN: usize = 16; // 80 bits
const MAGIC: &[u8; 4] = b"CDJ1";
const TAG_LEN: usize = 16;

pub const MAX_REQUEST: usize = 4096;
pub const MAX_RESPONSE: usize = 256 << 20;

#[derive(Clone)]
pub struct GroupKey([u8; 32]);

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("os rng");
    b
}

/// Fresh random code, formatted `XXXX-XXXX-XXXX-XXXX`.
pub fn new_code() -> String {
    // 256 is a multiple of 32, so masking keeps every symbol equally likely
    let raw: String = random::<CODE_LEN>()
        .iter()
        .map(|b| ALPHABET[(b & 31) as usize] as char)
        .collect();
    format_code(&raw)
}

/// Uppercase, drop separators, fold look-alikes (O→0, I/L→1).
/// None unless the input is a full code.
pub fn normalize(input: &str) -> Option<String> {
    let mut raw = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        };
        if !c.is_ascii() || !ALPHABET.contains(&(c as u8)) {
            return None;
        }
        raw.push(c);
    }
    (raw.len() == CODE_LEN).then(|| format_code(&raw))
}

fn format_code(raw: &str) -> String {
    raw.as_bytes()
        .chunks(4)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}

/// Random per-process id, so a machine skips its own beacons.
pub fn instance_id() -> String {
    random::<8>().iter().map(|b| format!("{:02x}", b)).collect()
}

fn cipher(key: [u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(&key))
}

impl GroupKey {
    pub fn from_code(code: &str) -> Option<GroupKey> {
        let raw = normalize(code)?.replace('-', "");
        let mut mac = <HmacSha256 as Mac>::new_from_slice(b"chat-daddy join v1").ok()?;
        mac.update(raw.as_bytes());
        Some(GroupKey(mac.finalize().into_bytes().into()))
    }

    fn derive(&self, label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.0).expect("hmac key");
        mac.update(label);
        for p in parts {
            mac.update(p);
        }
        mac.finalize().into_bytes().into()
    }

    pub fn seal_beacon(&self, plain: &[u8]) -> Vec<u8> {
        let nonce = random::<12>();
        let ct = cipher(self.derive(b"beacon", &[]))
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: MAGIC })
            .expect("seal");
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    pub fn open_beacon(&self, pkt: &[u8]) -> Option<Vec<u8>> {
        let rest = pkt.strip_prefix(MAGIC)?;
        if rest.len() < 12 + TAG_LEN {
            return None;
        }
        let (nonce, ct) = rest.split_at(12);
        cipher(self.derive(b"beacon", &[]))
            .decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: MAGIC })
            .ok()
    }
}

/// Sealed TCP session. Both sides contribute a nonce, so frames can't be
/// replayed across sessions; each direction has its own key, so they can't
/// be reflected. A peer proves the code by sending a frame that opens.
pub struct Channel {
    stream: TcpStream,
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_ctr: u64,
    recv_ctr: u64,
}

fn frame_nonce(ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&ctr.to_be_bytes());
    n
}

impl Channel {
    pub fn connect(mut stream: TcpStream, key: &GroupKey) -> Option<Channel> {
        let cn = random::<16>();
        let mut hello = MAGIC.to_vec();
        hello.extend_from_slice(&cn);
        stream.write_all(&hello).ok()?;
        let mut sn = [0u8; 16];
        stream.read_exact(&mut sn).ok()?;
        Some(Channel::new(stream, key, &cn, &sn, true))
    }

    pub fn accept(mut stream: TcpStream, key: &GroupKey) -> Option<Channel> {
        let mut hello = [0u8; 20];
        stream.read_exact(&mut hello).ok()?;
        if &hello[..4] != MAGIC {
            return None;
        }
        let sn = random::<16>();
        stream.write_all(&sn).ok()?;
        Some(Channel::new(stream, key, &hello[4..], &sn, false))
    }

    fn new(stream: TcpStream, key: &GroupKey, cn: &[u8], sn: &[u8], client: bool) -> Channel {
        let c2s = cipher(key.derive(b"c2s", &[cn, sn]));
        let s2c = cipher(key.derive(b"s2c", &[cn, sn]));
        let (send, recv) = if client { (c2s, s2c) } else { (s2c, c2s) };
        Channel { stream, send, recv, send_ctr: 0, recv_ctr: 0 }
    }

    pub fn send(&mut self, msg: &[u8]) -> Option<()> {
        let ct = self.send.encrypt(Nonce::from_slice(&frame_nonce(self.send_ctr)), msg).ok()?;
        self.send_ctr += 1;
        self.stream.write_all(&(ct.len() as u32).to_be_bytes()).ok()?;
        self.stream.write_all(&ct).ok()
    }

    pub fn recv(&mut self, max: usize) -> Option<Vec<u8>> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).ok()?;
        let len = u32::from_be_bytes(len) as usize;
        if len < TAG_LEN || len > max + TAG_LEN {
            return None;
        }
        // grows as bytes arrive, so a bogus length can't force a big allocation
        let mut ct = Vec::new();
        (&mut self.stream).take(len as u64).read_to_end(&mut ct).ok()?;
        if ct.len() != len {
            return None;
        }
        let pt = self.recv.decrypt(Nonce::from_slice(&frame_nonce(self.recv_ctr)), ct.as_slice()).ok()?;
        self.recv_ctr += 1;
        Some(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn code_roundtrip() {
        let c = new_code();
        assert_eq!(c.len(), 19);
        assert_eq!(normalize(&c.to_lowercase().replace('-', " ")), Some(c.clone()));
        assert_eq!(normalize("oooo-iiii-llll-0000"), Some("0000-1111-1111-0000".into()));
        assert_eq!(normalize("ABCD"), None);
        assert_eq!(normalize("ABCD-EFGH-JKMN-PQRU"), None); // U not in alphabet
    }

    #[test]
    fn beacon_needs_same_code() {
        let a = GroupKey::from_code(&new_code()).unwrap();
        let b = GroupKey::from_code(&new_code()).unwrap();
        let pkt = a.seal_beacon(b"hi");
        assert_eq!(a.open_beacon(&pkt).as_deref(), Some(&b"hi"[..]));
        assert!(b.open_beacon(&pkt).is_none());
    }

    fn session(server_code: &str, client_code: &str) -> Option<Vec<u8>> {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let sk = GroupKey::from_code(server_code).unwrap();
        let srv = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut ch = Channel::accept(s, &sk)?;
            let req = ch.recv(MAX_REQUEST)?;
            ch.send(&[req.as_slice(), b" ok"].concat())
        });
        let ck = GroupKey::from_code(client_code).unwrap();
        let mut ch = Channel::connect(TcpStream::connect(addr).unwrap(), &ck)?;
        ch.send(b"LIST")?;
        let resp = ch.recv(MAX_RESPONSE);
        let _ = srv.join();
        resp
    }

    #[test]
    fn channel_needs_same_code() {
        let code = new_code();
        assert_eq!(session(&code, &code).as_deref(), Some(&b"LIST ok"[..]));
        assert!(session(&code, &new_code()).is_none());
    }
}
