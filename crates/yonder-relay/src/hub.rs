//! Relay routing state: authenticated peers, host registry, links, presence watchers.
//! The hub never inspects binary payloads.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tokio::sync::mpsc;
use yonder_proto::keys::PublicKey;
use yonder_proto::relay::{Role, ServerMsg};

/// Messages queued to a peer's WebSocket writer.
#[derive(Debug)]
pub enum Out {
    Json(ServerMsg),
    Binary(Vec<u8>),
    /// Close the socket after flushing.
    Close,
}

pub type ConnId = u64;

pub struct Peer {
    pub role: Role,
    pub key: PublicKey,
    pub tx: mpsc::Sender<Out>,
    pub ip: IpAddr,
    /// Links this peer is an end of.
    pub links: HashSet<u32>,
    pub watching: Vec<PublicKey>,
    pub open_times: Vec<Instant>,
}

pub struct Link {
    pub device: ConnId,
    pub host: ConnId,
}

#[derive(Default)]
struct Inner {
    peers: HashMap<ConnId, Peer>,
    hosts: HashMap<PublicKey, ConnId>,
    links: HashMap<u32, Link>,
    watchers: HashMap<PublicKey, HashSet<ConnId>>,
    per_ip: HashMap<IpAddr, u32>,
}

pub struct Limits {
    pub max_links_per_device: usize,
    pub max_conns_per_ip: u32,
    pub opens_per_minute: usize,
    pub max_watch: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_links_per_device: 64, max_conns_per_ip: 32, opens_per_minute: 20, max_watch: 64 }
    }
}

pub struct Hub {
    inner: Mutex<Inner>,
    next_conn: AtomicU64,
    next_link: AtomicU32,
    pub limits: Limits,
}

impl Hub {
    pub fn new(limits: Limits) -> Self {
        Self { inner: Mutex::new(Inner::default()), next_conn: AtomicU64::new(1), next_link: AtomicU32::new(1), limits }
    }

    pub fn next_conn_id(&self) -> ConnId {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Count a new TCP/WebSocket connection from `ip`. Returns false when over the limit.
    pub fn acquire_ip(&self, ip: IpAddr) -> bool {
        let mut g = self.inner.lock().unwrap();
        let n = g.per_ip.entry(ip).or_insert(0);
        if *n >= self.limits.max_conns_per_ip {
            return false;
        }
        *n += 1;
        true
    }

    pub fn release_ip(&self, ip: IpAddr) {
        let mut g = self.inner.lock().unwrap();
        if let Some(n) = g.per_ip.get_mut(&ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                g.per_ip.remove(&ip);
            }
        }
    }

    /// Register an authenticated peer. For hosts, an existing connection with the same key
    /// is replaced (told `replaced` and closed).
    pub fn register(&self, id: ConnId, role: Role, key: PublicKey, tx: mpsc::Sender<Out>, ip: IpAddr) {
        let mut g = self.inner.lock().unwrap();
        let mut replaced = None;
        if role == Role::Host {
            if let Some(old) = g.hosts.insert(key, id) {
                replaced = Some(old);
            }
        }
        g.peers.insert(
            id,
            Peer { role, key, tx, ip, links: HashSet::new(), watching: Vec::new(), open_times: Vec::new() },
        );
        if let Some(old) = replaced {
            if let Some(p) = g.peers.get(&old) {
                let _ = p.tx.try_send(Out::Json(ServerMsg::Error {
                    code: "replaced".into(),
                    message: "another connection for this host key".into(),
                }));
                let _ = p.tx.try_send(Out::Close);
            }
            Self::drop_links_of(&mut g, old, "host_replaced");
        }
        if role == Role::Host {
            Self::notify_presence(&g, key, true);
        }
    }

    pub fn unregister(&self, id: ConnId) {
        let mut g = self.inner.lock().unwrap();
        let Some(peer) = g.peers.get(&id) else { return };
        let (role, key, watching) = (peer.role, peer.key, peer.watching.clone());
        Self::drop_links_of(&mut g, id, "peer_gone");
        for h in watching {
            if let Some(set) = g.watchers.get_mut(&h) {
                set.remove(&id);
                if set.is_empty() {
                    g.watchers.remove(&h);
                }
            }
        }
        g.peers.remove(&id);
        if role == Role::Host && g.hosts.get(&key) == Some(&id) {
            g.hosts.remove(&key);
            Self::notify_presence(&g, key, false);
        }
    }

    fn notify_presence(g: &Inner, host: PublicKey, online: bool) {
        if let Some(ws) = g.watchers.get(&host) {
            for w in ws {
                if let Some(p) = g.peers.get(w) {
                    let _ = p.tx.try_send(Out::Json(ServerMsg::Presence { host, online }));
                }
            }
        }
    }

    fn drop_links_of(g: &mut Inner, id: ConnId, reason: &str) {
        let links: Vec<u32> = g.peers.get(&id).map(|p| p.links.iter().copied().collect()).unwrap_or_default();
        for l in links {
            if let Some(link) = g.links.remove(&l) {
                let other = if link.device == id { link.host } else { link.device };
                if let Some(p) = g.peers.get_mut(&other) {
                    p.links.remove(&l);
                    let _ = p.tx.try_send(Out::Json(ServerMsg::Closed { link: l, reason: reason.into() }));
                }
            }
            if let Some(p) = g.peers.get_mut(&id) {
                p.links.remove(&l);
            }
        }
    }

    pub fn watch(&self, id: ConnId, hosts: Vec<PublicKey>) {
        let mut g = self.inner.lock().unwrap();
        let hosts: Vec<PublicKey> = hosts.into_iter().take(self.limits.max_watch).collect();
        let old = match g.peers.get_mut(&id) {
            Some(p) if p.role == Role::Device => std::mem::replace(&mut p.watching, hosts.clone()),
            _ => return,
        };
        for h in old {
            if let Some(set) = g.watchers.get_mut(&h) {
                set.remove(&id);
            }
        }
        for h in &hosts {
            g.watchers.entry(*h).or_default().insert(id);
        }
        if let Some(p) = g.peers.get(&id) {
            for h in hosts {
                let online = g.hosts.contains_key(&h);
                let _ = p.tx.try_send(Out::Json(ServerMsg::Presence { host: h, online }));
            }
        }
    }

    /// Device opens a link to a host.
    pub fn open(&self, id: ConnId, req: u32, to: PublicKey) {
        let mut g = self.inner.lock().unwrap();
        let limits = &self.limits;
        let Some(dev) = g.peers.get_mut(&id) else { return };
        let fail = |dev: &Peer, reason: &str| {
            let _ = dev.tx.try_send(Out::Json(ServerMsg::OpenFailed { req, to, reason: reason.into() }));
        };
        if dev.role != Role::Device {
            fail(dev, "invalid");
            return;
        }
        let now = Instant::now();
        dev.open_times.retain(|t| now.duration_since(*t).as_secs() < 60);
        if dev.open_times.len() >= limits.opens_per_minute {
            fail(dev, "rate_limited");
            return;
        }
        dev.open_times.push(now);
        if dev.links.len() >= limits.max_links_per_device {
            fail(dev, "too_many_links");
            return;
        }
        let dev_key = dev.key;
        let Some(&host_id) = g.hosts.get(&to) else {
            let dev = g.peers.get(&id).unwrap();
            fail(dev, "host_offline");
            return;
        };
        let mut link = self.next_link.fetch_add(1, Ordering::Relaxed);
        while link == 0 || g.links.contains_key(&link) {
            link = self.next_link.fetch_add(1, Ordering::Relaxed);
        }
        g.links.insert(link, Link { device: id, host: host_id });
        if let Some(h) = g.peers.get_mut(&host_id) {
            h.links.insert(link);
            let _ = h.tx.try_send(Out::Json(ServerMsg::Incoming { link, from: dev_key }));
        }
        if let Some(d) = g.peers.get_mut(&id) {
            d.links.insert(link);
            let _ = d.tx.try_send(Out::Json(ServerMsg::Opened { req, link, to }));
        }
    }

    pub fn close(&self, id: ConnId, link: u32) {
        let mut g = self.inner.lock().unwrap();
        let Some(l) = g.links.get(&link) else { return };
        if l.device != id && l.host != id {
            return;
        }
        let l = g.links.remove(&link).unwrap();
        let other = if l.device == id { l.host } else { l.device };
        if let Some(p) = g.peers.get_mut(&id) {
            p.links.remove(&link);
        }
        if let Some(p) = g.peers.get_mut(&other) {
            p.links.remove(&link);
            let _ = p.tx.try_send(Out::Json(ServerMsg::Closed { link, reason: "peer_closed".into() }));
        }
    }

    /// Forward a binary frame to the other end of `link`.
    pub fn forward(&self, id: ConnId, link: u32, frame: Vec<u8>) -> ForwardResult {
        let g = self.inner.lock().unwrap();
        let Some(l) = g.links.get(&link) else { return ForwardResult::UnknownLink };
        let other = if l.device == id {
            l.host
        } else if l.host == id {
            l.device
        } else {
            return ForwardResult::UnknownLink;
        };
        let Some(p) = g.peers.get(&other) else { return ForwardResult::UnknownLink };
        match p.tx.try_send(Out::Binary(frame)) {
            Ok(()) => ForwardResult::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => ForwardResult::PeerSlow(other),
            Err(mpsc::error::TrySendError::Closed(_)) => ForwardResult::Sent,
        }
    }

    /// Ask a slow peer to disconnect (its queue is full).
    pub fn kick(&self, id: ConnId) {
        let g = self.inner.lock().unwrap();
        if let Some(p) = g.peers.get(&id) {
            let tx = p.tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(Out::Close).await;
            });
        }
    }

    pub fn stats(&self) -> (usize, usize, usize) {
        let g = self.inner.lock().unwrap();
        (g.peers.len(), g.hosts.len(), g.links.len())
    }
}

pub enum ForwardResult {
    Sent,
    /// The receiver's queue is full; it should be disconnected.
    PeerSlow(ConnId),
    /// The link does not exist or the sender is not an end of it.
    UnknownLink,
}
