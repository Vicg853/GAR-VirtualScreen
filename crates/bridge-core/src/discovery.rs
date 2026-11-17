//! LAN discovery. One record per host: screens are slots listed inside it, so a
//! removed screen disappears with the next record instead of lingering as its
//! own stale service.
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const SERVICE: &str = "_airbridge._udp.local.";
/// A TXT value is one length-prefixed string of at most 255 bytes.
const TXT_LIMIT: usize = 250;
const NAME_LIMIT: usize = 24;

/// A temporary screen a viewer may join with its PIN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredScreen {
    pub screen_id: u32,
    pub name: String,
}

/// An active session on a host. Carries no secret: logging in still needs the password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub username: String,
    pub name: String,
    pub screens: u8,
    /// False while the host has paused its side of auto-connect.
    pub auto_connect: bool,
    /// First eight characters of the session ID, enough to match a saved login.
    pub id_prefix: String,
}

/// What a host announces. The whole list replaces the previous one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostAdvert {
    pub label: String,
    pub host_id: String,
    pub screens: Vec<DiscoveredScreen>,
    pub sessions: Vec<DiscoveredSession>,
}

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    /// mDNS instance name; identifies the record, never shown to the user.
    pub name: String,
    pub label: String,
    pub addresses: Vec<SocketAddr>,
    pub host_id: String,
    pub screens: Vec<DiscoveredScreen>,
    pub sessions: Vec<DiscoveredSession>,
}

impl DiscoveredHost {
    /// Whether this host currently offers the saved session for automatic connection.
    pub fn offers(&self, host_id: &str, session_id: &str) -> Option<&DiscoveredSession> {
        (self.host_id == host_id)
            .then(|| {
                self.sessions
                    .iter()
                    .find(|s| session_id.starts_with(&s.id_prefix) && !s.id_prefix.is_empty())
            })
            .flatten()
    }
}

fn clean(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '|' | '=' | '~') && !c.is_control())
        .take(NAME_LIMIT)
        .collect()
}

/// Join entries while the value still fits one TXT string; later entries are dropped.
fn join(entries: impl Iterator<Item = String>) -> String {
    let mut value = String::new();
    for entry in entries {
        if value.len() + entry.len() + 1 > TXT_LIMIT {
            break;
        }
        if !value.is_empty() {
            value.push('|');
        }
        value.push_str(&entry);
    }
    value
}

fn encode_screens(screens: &[DiscoveredScreen]) -> String {
    join(
        screens
            .iter()
            .map(|s| format!("{}={}", s.screen_id, clean(&s.name))),
    )
}

fn decode_screens(value: &str) -> Vec<DiscoveredScreen> {
    value
        .split('|')
        .filter_map(|entry| {
            let (id, name) = entry.split_once('=')?;
            let screen_id = id.parse().ok().filter(|id| *id != 0)?;
            Some(DiscoveredScreen {
                screen_id,
                name: clean(name),
            })
        })
        .take(crate::protocol::MAX_SCREENS)
        .collect()
}

fn encode_sessions(sessions: &[DiscoveredSession]) -> String {
    join(sessions.iter().map(|s| {
        format!(
            "{}~{}~{}~{}~{}",
            clean(&s.username),
            clean(&s.name),
            s.screens,
            u8::from(s.auto_connect),
            clean(&s.id_prefix)
        )
    }))
}

fn decode_sessions(value: &str) -> Vec<DiscoveredSession> {
    value
        .split('|')
        .filter_map(|entry| {
            let mut parts = entry.split('~');
            let username = clean(parts.next()?);
            let name = clean(parts.next()?);
            let screens = parts.next()?.parse().ok()?;
            let auto_connect = parts.next()? == "1";
            let id_prefix = clean(parts.next()?);
            (!username.is_empty()).then_some(DiscoveredSession {
                username,
                name,
                screens,
                auto_connect,
                id_prefix,
            })
        })
        .take(32)
        .collect()
}

pub struct Advertiser {
    daemon: ServiceDaemon,
    port: u16,
    current: std::sync::Mutex<Option<HostAdvert>>,
}

impl Advertiser {
    pub fn new(port: u16) -> Result<Self> {
        Ok(Self {
            daemon: ServiceDaemon::new()?,
            port,
            current: Default::default(),
        })
    }

    /// Announce the host's current slot list. Unchanged lists are not re-sent.
    pub fn publish(&self, advert: &HostAdvert) -> Result<()> {
        let mut current = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("Discovery state unavailable"))?;
        if current.as_ref() == Some(advert) {
            return Ok(());
        }
        let instance = format!(
            "AirBridge {}",
            advert.host_id.get(..8).unwrap_or(&advert.host_id)
        );
        let service = ServiceInfo::new(
            SERVICE,
            &instance,
            "airbridge.local.",
            (),
            self.port,
            &[
                ("version", crate::protocol::VERSION.to_string()),
                ("host", advert.host_id.clone()),
                ("name", clean(&advert.label)),
                ("temp", encode_screens(&advert.screens)),
                ("sessions", encode_sessions(&advert.sessions)),
            ][..],
        )?
        .enable_addr_auto();
        // Registering the same instance again re-announces it with the new record.
        self.daemon.register(service)?;
        *current = Some(advert.clone());
        Ok(())
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(DiscoveredHost),
    Lost(String),
}

pub struct DiscoveryBrowser {
    receive: mpsc::Receiver<DiscoveryEvent>,
    cancel: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}
impl DiscoveryBrowser {
    pub fn recv_timeout(
        &self,
        duration: Duration,
    ) -> std::result::Result<DiscoveryEvent, mpsc::RecvTimeoutError> {
        self.receive.recv_timeout(duration)
    }
}
impl Drop for DiscoveryBrowser {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

pub fn browse(cancel: Arc<AtomicBool>) -> Result<DiscoveryBrowser> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE)?;
    let (send, receive) = mpsc::channel();
    let worker_cancel = cancel.clone();
    let task = thread::spawn(move || {
        while !worker_cancel.load(Ordering::Acquire) {
            let event = match events.recv_timeout(Duration::from_millis(250)) {
                Ok(event) => event,
                Err(mdns_sd::RecvTimeoutError::Timeout) => continue,
                Err(_) => break,
            };
            if let ServiceEvent::ServiceRemoved(_, name) = &event
                && send.send(DiscoveryEvent::Lost(name.clone())).is_err()
            {
                break;
            }
            if let ServiceEvent::ServiceResolved(info) = event {
                if info
                    .get_property_val_str("version")
                    .and_then(|v| v.parse::<u16>().ok())
                    != Some(crate::protocol::VERSION)
                {
                    continue;
                }

                let mut addresses: Vec<_> = info
                    .get_addresses()
                    .iter()
                    .map(|ip| ip.to_ip_addr())
                    .filter(|ip| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast())
                    .map(|ip| SocketAddr::new(ip, info.get_port()))
                    .collect();
                if addresses.is_empty() {
                    continue;
                }
                addresses.sort_by_key(|address| !address.is_ipv4());
                let host_id = info
                    .get_property_val_str("host")
                    .unwrap_or("unknown")
                    .to_owned();
                let label = info
                    .get_property_val_str("name")
                    .map(clean)
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("Host {}", host_id.get(..8).unwrap_or(&host_id)));
                if send
                    .send(DiscoveryEvent::Found(DiscoveredHost {
                        name: info.get_fullname().to_owned(),
                        label,
                        addresses,
                        host_id,
                        screens: decode_screens(info.get_property_val_str("temp").unwrap_or("")),
                        sessions: decode_sessions(
                            info.get_property_val_str("sessions").unwrap_or(""),
                        ),
                    }))
                    .is_err()
                {
                    break;
                }
            }
        }
        let _ = daemon.stop_browse(SERVICE);
        if let Ok(stopped) = daemon.shutdown() {
            let _ = stopped.recv_timeout(Duration::from_secs(1));
        }
    });
    Ok(DiscoveryBrowser {
        receive,
        cancel,
        task: Some(task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_lists_roundtrip_and_stay_inside_one_txt_string() {
        let screens = (1..=8)
            .map(|screen_id| DiscoveredScreen {
                screen_id,
                name: format!("Temp|{screen_id}=a very long temporary screen name"),
            })
            .collect::<Vec<_>>();
        let encoded = encode_screens(&screens);
        assert!(encoded.len() <= TXT_LIMIT);
        let decoded = decode_screens(&encoded);
        assert_eq!(decoded.len(), 8);
        assert_eq!(decoded[0].screen_id, 1);
        assert!(decoded[0].name.len() <= NAME_LIMIT);
        assert!(!decoded[0].name.contains('|'));
        assert!(decode_screens("").is_empty());
        assert!(
            decode_screens("0=physical|x=bad").is_empty(),
            "screen 0 is never a slot"
        );

        let sessions = vec![DiscoveredSession {
            username: "studio-desk".into(),
            name: "Studio".into(),
            screens: 2,
            auto_connect: false,
            id_prefix: "0123abcd".into(),
        }];
        assert_eq!(decode_sessions(&encode_sessions(&sessions)), sessions);
        let host = DiscoveredHost {
            name: "x".into(),
            label: "Mac".into(),
            addresses: vec![],
            host_id: "host".into(),
            screens: vec![],
            sessions,
        };
        assert!(host.offers("host", "0123abcdffff").is_some());
        assert!(host.offers("other", "0123abcdffff").is_none());
        assert!(host.offers("host", "ffff").is_none());
    }
}
