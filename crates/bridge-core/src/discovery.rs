use crate::protocol::PORT;
use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::{net::SocketAddr, sync::mpsc, thread};

const SERVICE: &str = "_airbridge._udp.local.";

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub name: String,
    pub addresses: Vec<SocketAddr>,
}

pub struct Advertiser {
    daemon: ServiceDaemon,
}

impl Advertiser {
    pub fn new(name: &str) -> Result<Self> {
        let daemon = ServiceDaemon::new()?;
        let service = ServiceInfo::new(
            SERVICE,
            name,
            "airbridge.local.",
            (),
            PORT,
            &[("version", "1")][..],
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

pub fn browse() -> Result<mpsc::Receiver<DiscoveredHost>> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE)?;
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(event) = events.recv() {
            if let ServiceEvent::ServiceResolved(info) = event {
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
                    .send(DiscoveredHost {
                        name: info.get_fullname().to_owned(),
                        addresses,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
        let _ = daemon.shutdown();
    });
    Ok(receive)
}
