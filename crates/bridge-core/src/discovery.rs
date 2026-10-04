use crate::protocol::PORT;
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

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub name: String,
    pub addresses: Vec<SocketAddr>,
    pub screen_id: u32,
    pub host_id: String,
}

pub struct Advertiser {
    daemon: ServiceDaemon,
}

impl Advertiser {
    pub fn new(name: &str) -> Result<Self> {
        Self::screen(name, 1, "legacy")
    }
    pub fn screen(name: &str, screen_id: u32, host_id: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let service = ServiceInfo::new(
            SERVICE,
            name,
            "airbridge.local.",
            (),
            PORT,
            &[
                ("version", crate::protocol::VERSION.to_string()),
                ("screen", screen_id.to_string()),
                ("host", host_id.to_owned()),
            ][..],
        )?
        .enable_addr_auto();
        daemon.register(service)?;
        Ok(Self { daemon })
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
                if send
                    .send(DiscoveryEvent::Found(DiscoveredHost {
                        name: info.get_fullname().to_owned(),
                        addresses,
                        screen_id: info
                            .get_property_val_str("screen")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1),
                        host_id: info
                            .get_property_val_str("host")
                            .unwrap_or("unknown")
                            .to_owned(),
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
