use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use horus_registry::{RateLimiter, Registry, RegistryError, StakeBook};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use x25519_dalek::{PublicKey, StaticSecret};

/// Dev relay static key (must match protocol `dev_relay_public_key`).
pub fn relay_secret() -> StaticSecret {
    StaticSecret::from([42u8; 32])
}

/// How long a polled message stays leased before redelivery if never ACKed.
const DEFAULT_LEASE_MS: u64 = 5 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayMessage {
    pub body: Vec<u8>,
    expires_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Lease {
    queue: String,
    msg: RelayMessage,
    lease_until_ms: u64,
}

#[derive(Debug)]
pub struct Relay {
    ttl: Duration,
    lease_ms: u64,
    queues: HashMap<String, VecDeque<RelayMessage>>,
    /// Messages handed out via poll awaiting client ACK (key = lease id).
    leases: HashMap<String, Lease>,
    pub registry: Registry,
    pub stakes: StakeBook,
    limiter: RateLimiter,
    /// Stricter limiter for @ claims (opaque nullifier / username key).
    claim_limiter: RateLimiter,
    persist_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Default)]
struct Snap {
    queues: HashMap<String, Vec<RelayMessage>>,
    #[serde(default)]
    leases: HashMap<String, Lease>,
    #[serde(default)]
    registry: Vec<horus_registry::RegistryEntry>,
}

impl Relay {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            lease_ms: DEFAULT_LEASE_MS,
            queues: HashMap::new(),
            leases: HashMap::new(),
            registry: Registry::new(),
            stakes: StakeBook::new(),
            limiter: RateLimiter::new(Duration::from_secs(60), 120),
            claim_limiter: RateLimiter::new(Duration::from_secs(60 * 60), 5),
            persist_path: None,
        }
    }

    pub fn with_persist(ttl: Duration, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut relay = Self::new(ttl);
        relay.persist_path = Some(path.clone());
        if path.exists() {
            let _ = relay.load(&path);
        }
        relay
    }

    pub fn send(&mut self, queue: &str, body: Vec<u8>) -> Result<(), RelayError> {
        if queue.trim().is_empty() {
            return Err(RelayError::EmptyQueue);
        }
        if body.is_empty() {
            return Err(RelayError::EmptyBody);
        }
        const MAX_BODY: usize = 256 * 1024;
        if body.len() > MAX_BODY {
            return Err(RelayError::PayloadTooLarge);
        }
        if self.limiter.check(queue).is_err() {
            return Err(RelayError::RateLimited);
        }

        self.prune();
        let expires_unix_ms = now_ms() + self.ttl.as_millis() as u64;
        self.queues
            .entry(queue.to_string())
            .or_default()
            .push_back(RelayMessage {
                body,
                expires_unix_ms,
            });
        self.save();
        Ok(())
    }

    /// Lease the next message (does not delete). Client must `ack` or it redelivers.
    /// Returns `(lease_id, body)`.
    pub fn poll(&mut self, queue: &str) -> Option<(String, Vec<u8>)> {
        self.prune();
        let q = self.queues.get_mut(queue)?;
        let msg = q.pop_front()?;
        let id = lease_id(queue, &msg.body);
        let lease_until_ms = now_ms() + self.lease_ms;
        self.leases.insert(
            id.clone(),
            Lease {
                queue: queue.to_string(),
                msg: msg.clone(),
                lease_until_ms,
            },
        );
        self.save();
        Some((id, msg.body))
    }

    /// Confirm delivery; removes the leased message permanently.
    pub fn ack(&mut self, lease_id: &str) -> bool {
        self.prune();
        let removed = self.leases.remove(lease_id).is_some();
        if removed {
            self.save();
        }
        removed
    }

    pub fn len(&mut self, queue: &str) -> usize {
        self.prune();
        let queued = self.queues.get(queue).map_or(0, VecDeque::len);
        let leased = self
            .leases
            .values()
            .filter(|l| l.queue == queue)
            .count();
        queued + leased
    }

    pub fn claim_username(
        &mut self,
        username: &str,
        salt: &[u8],
        findable: bool,
        contact: Option<&str>,
    ) -> Result<(), ClaimError> {
        let key = horus_registry::nullifier_for_username(username)
            .unwrap_or_else(|| format!("bad:{username}"));
        if self.claim_limiter.check(&key).is_err() {
            return Err(ClaimError::RateLimited);
        }
        let r = self.registry.claim(username, salt, findable, contact);
        match r {
            Ok(_) => {
                self.save();
                Ok(())
            }
            Err(e) => Err(ClaimError::Registry(e)),
        }
    }

    pub fn resolve_username(&self, username: &str) -> Option<&str> {
        self.registry.resolve(username)
    }

    pub fn username_taken(&self, username: &str) -> bool {
        self.registry.is_taken(username)
    }

    /// Unwrap one onion layer and enqueue the inner blob on `next_queue`.
    pub fn forward_onion(&mut self, packet: &[u8]) -> Result<String, RelayError> {
        let (next_queue, body) = unwrap_onion(&relay_secret(), packet)?;
        self.send(&next_queue, body)?;
        Ok(next_queue)
    }

    pub fn prune(&mut self) {
        let now = now_ms();
        self.queues.retain(|_, queue| {
            queue.retain(|msg| msg.expires_unix_ms > now);
            !queue.is_empty()
        });
        // Expired leases → redeliver to front of queue.
        let expired: Vec<String> = self
            .leases
            .iter()
            .filter(|(_, l)| l.lease_until_ms <= now || l.msg.expires_unix_ms <= now)
            .map(|(k, _)| k.clone())
            .collect();
        for id in expired {
            if let Some(lease) = self.leases.remove(&id) {
                if lease.msg.expires_unix_ms > now {
                    self.queues
                        .entry(lease.queue)
                        .or_default()
                        .push_front(lease.msg);
                }
            }
        }
    }

    fn save(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let snap = Snap {
            queues: self
                .queues
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            leases: self.leases.clone(),
            registry: self.registry.entries(),
        };
        if let Ok(json) = serde_json::to_vec(&snap) {
            let _ = fs::write(path, json);
        }
    }

    fn load(&mut self, path: &Path) -> Result<(), String> {
        let raw = fs::read(path).map_err(|e| e.to_string())?;
        let snap: Snap = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        self.queues = snap
            .queues
            .into_iter()
            .map(|(k, v)| (k, VecDeque::from(v)))
            .collect();
        self.leases = snap.leases;
        self.registry.load_entries(snap.registry);
        self.prune();
        Ok(())
    }
}

fn lease_id(queue: &str, body: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(queue.as_bytes());
    h.update(b"|");
    h.update(body);
    h.update(b"|");
    h.update(now_ms().to_be_bytes());
    // Hex — safe in URL paths (base64 would include `/`).
    format!("{:x}", h.finalize())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ClaimError {
    Registry(RegistryError),
    RateLimited,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RelayError {
    EmptyQueue,
    EmptyBody,
    PayloadTooLarge,
    BadOnion,
    RateLimited,
}

#[derive(Deserialize)]
struct OnionLayer {
    next_queue: String,
    body_b64: String,
}

fn unwrap_onion(sk: &StaticSecret, packet: &[u8]) -> Result<(String, Vec<u8>), RelayError> {
    if packet.len() < 45 {
        return Err(RelayError::BadOnion);
    }
    let (eph, ct) = packet.split_at(32);
    let mut eph_arr = [0u8; 32];
    eph_arr.copy_from_slice(eph);
    let shared = sk.diffie_hellman(&PublicKey::from(eph_arr));
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
    let mut key = [0u8; 32];
    hk.expand(b"horus-onion-v1", &mut key)
        .map_err(|_| RelayError::BadOnion)?;
    if ct.len() < 13 {
        return Err(RelayError::BadOnion);
    }
    let (nonce, body) = ct.split_at(12);
    let cipher = ChaCha20Poly1305::new((&key).into());
    let plain = cipher
        .decrypt(Nonce::from_slice(nonce), body)
        .map_err(|_| RelayError::BadOnion)?;
    let layer: OnionLayer = serde_json::from_slice(&plain).map_err(|_| RelayError::BadOnion)?;
    let inner = STANDARD
        .decode(&layer.body_b64)
        .map_err(|_| RelayError::BadOnion)?;
    Ok((layer.next_queue, inner))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn send_then_poll_leases_until_ack() {
        let mut relay = Relay::new(Duration::from_secs(30));
        relay.send("q1", b"cipher".to_vec()).unwrap();

        assert_eq!(relay.len("q1"), 1);
        let (id, body) = relay.poll("q1").unwrap();
        assert_eq!(body, b"cipher");
        // Still counted while leased.
        assert_eq!(relay.len("q1"), 1);
        assert_eq!(relay.poll("q1"), None);
        assert!(relay.ack(&id));
        assert_eq!(relay.len("q1"), 0);
        assert!(!relay.ack(&id));
    }

    #[test]
    fn expired_lease_redelivers() {
        let mut relay = Relay::new(Duration::from_secs(30));
        relay.lease_ms = 5;
        relay.send("q", b"again".to_vec()).unwrap();
        let (_id, body) = relay.poll("q").unwrap();
        assert_eq!(body, b"again");
        thread::sleep(Duration::from_millis(15));
        let (_id2, body2) = relay.poll("q").unwrap();
        assert_eq!(body2, b"again");
    }

    #[test]
    fn keeps_queues_separate() {
        let mut relay = Relay::new(Duration::from_secs(30));
        relay.send("a", b"one".to_vec()).unwrap();
        relay.send("b", b"two".to_vec()).unwrap();

        assert_eq!(relay.poll("b").map(|(_, b)| b), Some(b"two".to_vec()));
        assert_eq!(relay.poll("a").map(|(_, b)| b), Some(b"one".to_vec()));
    }

    #[test]
    fn ttl_prunes_expired_messages() {
        let mut relay = Relay::new(Duration::from_millis(5));
        relay.send("q", b"old".to_vec()).unwrap();
        thread::sleep(Duration::from_millis(10));

        assert_eq!(relay.poll("q"), None);
    }

    #[test]
    fn rejects_bad_inputs() {
        let mut relay = Relay::new(Duration::from_secs(30));

        assert_eq!(relay.send("", b"x".to_vec()), Err(RelayError::EmptyQueue));
        assert_eq!(relay.send("q", Vec::new()), Err(RelayError::EmptyBody));
    }

    #[test]
    fn registry_claim_resolve() {
        let mut relay = Relay::new(Duration::from_secs(30));
        let invite = "horus://invite/testdata";
        relay
            .claim_username("alice", b"salt-alice-aaaaaa", true, Some(invite))
            .unwrap();
        assert_eq!(relay.resolve_username("alice"), Some(invite));
        assert!(relay.username_taken("alice"));
        assert!(matches!(
            relay.claim_username("alice", b"salt-alice-aaaaaa", true, Some(invite)),
            Err(ClaimError::Registry(RegistryError::Taken))
        ));
    }

    #[test]
    fn registry_claim_is_rate_limited() {
        let mut relay = Relay::new(Duration::from_secs(30));
        let invite = "horus://invite/testdata";
        relay
            .claim_username("limited", b"salt-limit-aaaaaa", true, Some(invite))
            .unwrap();
        // Limiter runs before claim; repeated attempts on same name burn the window.
        for _ in 0..4 {
            let _ = relay.claim_username("limited", b"salt-limit-aaaaaa", true, Some(invite));
        }
        assert_eq!(
            relay.claim_username("limited", b"salt-limit-aaaaaa", true, Some(invite)),
            Err(ClaimError::RateLimited)
        );
    }

    #[test]
    fn persists_queues_and_registry() {
        let dir = std::env::temp_dir().join(format!("horus-relay-{}", now_ms()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("state.json");

        {
            let mut relay = Relay::with_persist(Duration::from_secs(60), &path);
            relay.send("q", b"blob".to_vec()).unwrap();
            relay
                .claim_username("bob", b"salt-bob-bbbbbbbb", true, Some("horus://invite/bob"))
                .unwrap();
        }

        let mut relay = Relay::with_persist(Duration::from_secs(60), &path);
        let (id, body) = relay.poll("q").unwrap();
        assert_eq!(body, b"blob");
        assert!(relay.ack(&id));
        assert_eq!(relay.resolve_username("bob"), Some("horus://invite/bob"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn onion_forward() {
        use base64::Engine;
        use chacha20poly1305::aead::{Aead, KeyInit};
        use chacha20poly1305::{ChaCha20Poly1305, Nonce};
        use hkdf::Hkdf;
        use rand::RngCore;
        use sha2::Sha256;
        use x25519_dalek::PublicKey;

        let mut sk_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut sk_bytes);
        let eph = StaticSecret::from(sk_bytes);
        let eph_pub = PublicKey::from(&eph);
        let shared = eph.diffie_hellman(&PublicKey::from(&relay_secret()));
        let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(b"horus-onion-v1", &mut key).unwrap();
        let layer = serde_json::json!({
            "next_queue": "final",
            "body_b64": STANDARD.encode(b"payload")
        });
        let cipher = ChaCha20Poly1305::new((&key).into());
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), layer.to_string().as_bytes())
            .unwrap();
        let mut packet = Vec::new();
        packet.extend_from_slice(eph_pub.as_bytes());
        packet.extend_from_slice(&nonce);
        packet.extend_from_slice(&ct);

        let mut relay = Relay::new(Duration::from_secs(30));
        assert_eq!(relay.forward_onion(&packet).unwrap(), "final");
        let (id, body) = relay.poll("final").unwrap();
        assert_eq!(body, b"payload");
        assert!(relay.ack(&id));
    }
}
