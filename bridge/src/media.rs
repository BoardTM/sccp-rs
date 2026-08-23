//! Encoded RTP/RTCP routing.
//!
//! The relay never decodes media. Every packet crossing a thread boundary uses
//! a bounded SPSC `rtrb`; a slow destination therefore drops packets instead of
//! accumulating latency.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use ipnet::Ipv4Net;
use rtrb::RingBuffer;
use thiserror::Error;

const MAX_DATAGRAM: usize = 2048;
const QUEUE_PACKETS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaMode {
    Direct,
    Relay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectMediaRoute {
    pub phones: Vec<Ipv4Net>,
    pub sip: Vec<Ipv4Net>,
}

#[derive(Clone, Debug, Default)]
pub struct MediaPolicy {
    direct_routes: Vec<DirectMediaRoute>,
}

impl MediaPolicy {
    pub fn new(direct_routes: Vec<DirectMediaRoute>) -> Self {
        Self { direct_routes }
    }

    pub fn select(
        &self,
        phone: Ipv4Addr,
        sip_peer: Ipv4Addr,
        compatible_payloads: bool,
    ) -> MediaMode {
        if compatible_payloads
            && self.direct_routes.iter().any(|route| {
                route.phones.iter().any(|network| network.contains(&phone))
                    && route.sip.iter().any(|network| network.contains(&sip_peer))
            })
        {
            MediaMode::Direct
        } else {
            MediaMode::Relay
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelayAddresses {
    /// Address advertised to the SCCP handset.
    pub phone_facing_rtp: SocketAddr,
    pub phone_facing_rtcp: SocketAddr,
    /// Address advertised in SIP SDP.
    pub sip_facing_rtp: SocketAddr,
    pub sip_facing_rtcp: SocketAddr,
}

#[derive(Clone, Debug, Default)]
pub struct RelayStats {
    phone_to_sip_packets: Arc<AtomicU64>,
    sip_to_phone_packets: Arc<AtomicU64>,
    phone_to_sip_drops: Arc<AtomicU64>,
    sip_to_phone_drops: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RelayStatsSnapshot {
    pub phone_to_sip_packets: u64,
    pub sip_to_phone_packets: u64,
    pub phone_to_sip_drops: u64,
    pub sip_to_phone_drops: u64,
}

impl RelayStats {
    pub fn snapshot(&self) -> RelayStatsSnapshot {
        RelayStatsSnapshot {
            phone_to_sip_packets: self.phone_to_sip_packets.load(Ordering::Relaxed),
            sip_to_phone_packets: self.sip_to_phone_packets.load(Ordering::Relaxed),
            phone_to_sip_drops: self.phone_to_sip_drops.load(Ordering::Relaxed),
            sip_to_phone_drops: self.sip_to_phone_drops.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("media port range must start on an even port and contain at least four RTP/RTCP pairs")]
    InvalidPortRange,
    #[error("unable to bind media sockets in configured range")]
    PortsExhausted,
    #[error("RTP relay I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("RTP relay supports IPv4 endpoints only")]
    Ipv4Required,
}

pub struct PortAllocator {
    bind_address: Ipv4Addr,
    advertised_address: Ipv4Addr,
    range: RangeInclusive<u16>,
    next: AtomicU16,
}

impl PortAllocator {
    pub fn new(
        bind_address: Ipv4Addr,
        advertised_address: Ipv4Addr,
        range: RangeInclusive<u16>,
    ) -> Result<Self, RelayError> {
        let start = *range.start();
        let end = *range.end();
        if !start.is_multiple_of(2) || end.saturating_sub(start) < 7 {
            return Err(RelayError::InvalidPortRange);
        }
        Ok(Self {
            bind_address,
            advertised_address,
            range,
            next: AtomicU16::new(start),
        })
    }

    fn allocate_pair(&self) -> Result<(UdpSocket, UdpSocket, SocketAddr, SocketAddr), RelayError> {
        let start = *self.range.start();
        let end = *self.range.end();
        let attempts = ((end - start) / 2) as usize + 1;
        for _ in 0..attempts {
            let mut port = self.next.fetch_add(2, Ordering::Relaxed);
            if port < start || port.saturating_add(1) > end {
                self.next.store(start.saturating_add(2), Ordering::Relaxed);
                port = start;
            }
            let rtp = match UdpSocket::bind((self.bind_address, port)) {
                Ok(socket) => socket,
                Err(_) => continue,
            };
            let rtcp = match UdpSocket::bind((self.bind_address, port + 1)) {
                Ok(socket) => socket,
                Err(_) => continue,
            };
            rtp.set_read_timeout(Some(Duration::from_millis(100)))?;
            rtcp.set_read_timeout(Some(Duration::from_millis(100)))?;
            return Ok((
                rtp,
                rtcp,
                SocketAddr::new(IpAddr::V4(self.advertised_address), port),
                SocketAddr::new(IpAddr::V4(self.advertised_address), port + 1),
            ));
        }
        Err(RelayError::PortsExhausted)
    }

    pub fn allocate(&self) -> Result<AllocatedRelay, RelayError> {
        let (phone_rtp, phone_rtcp, phone_facing_rtp, phone_facing_rtcp) = self.allocate_pair()?;
        let (sip_rtp, sip_rtcp, sip_facing_rtp, sip_facing_rtcp) = self.allocate_pair()?;
        Ok(AllocatedRelay {
            addresses: RelayAddresses {
                phone_facing_rtp,
                phone_facing_rtcp,
                sip_facing_rtp,
                sip_facing_rtcp,
            },
            phone_rtp,
            phone_rtcp,
            sip_rtp,
            sip_rtcp,
        })
    }
}

pub struct AllocatedRelay {
    pub addresses: RelayAddresses,
    phone_rtp: UdpSocket,
    phone_rtcp: UdpSocket,
    sip_rtp: UdpSocket,
    sip_rtcp: UdpSocket,
}

impl AllocatedRelay {
    pub fn start(self, phone: SocketAddr, sip_peer: SocketAddr) -> Result<RtpRelay, RelayError> {
        let SocketAddr::V4(phone) = phone else {
            return Err(RelayError::Ipv4Required);
        };
        let SocketAddr::V4(sip_peer) = sip_peer else {
            return Err(RelayError::Ipv4Required);
        };
        let destinations = RelayDestinations::new(phone, sip_peer);
        let stop = Arc::new(AtomicBool::new(false));
        let stats = RelayStats::default();
        let mut workers = Vec::with_capacity(8);

        let spawn_result = (|| -> Result<(), RelayError> {
            spawn_direction(
                &mut workers,
                "rtp-phone-to-sip",
                self.phone_rtp.try_clone()?,
                self.sip_rtp.try_clone()?,
                destinations.sip_rtp.clone(),
                stop.clone(),
                DirectionCounters {
                    packets: stats.phone_to_sip_packets.clone(),
                    drops: stats.phone_to_sip_drops.clone(),
                },
            )?;
            spawn_direction(
                &mut workers,
                "rtp-sip-to-phone",
                self.sip_rtp,
                self.phone_rtp,
                destinations.phone_rtp.clone(),
                stop.clone(),
                DirectionCounters {
                    packets: stats.sip_to_phone_packets.clone(),
                    drops: stats.sip_to_phone_drops.clone(),
                },
            )?;
            spawn_direction(
                &mut workers,
                "rtcp-phone-to-sip",
                self.phone_rtcp.try_clone()?,
                self.sip_rtcp.try_clone()?,
                destinations.sip_rtcp.clone(),
                stop.clone(),
                DirectionCounters {
                    packets: stats.phone_to_sip_packets.clone(),
                    drops: stats.phone_to_sip_drops.clone(),
                },
            )?;
            spawn_direction(
                &mut workers,
                "rtcp-sip-to-phone",
                self.sip_rtcp,
                self.phone_rtcp,
                destinations.phone_rtcp.clone(),
                stop.clone(),
                DirectionCounters {
                    packets: stats.sip_to_phone_packets.clone(),
                    drops: stats.sip_to_phone_drops.clone(),
                },
            )?;
            Ok(())
        })();
        if let Err(error) = spawn_result {
            stop.store(true, Ordering::Release);
            for worker in workers {
                let _ = worker.join();
            }
            return Err(error);
        }

        Ok(RtpRelay {
            addresses: self.addresses,
            stats,
            stop,
            workers,
            destinations,
        })
    }
}

#[derive(Clone)]
struct AtomicSocketAddrV4(Arc<AtomicU64>);

impl AtomicSocketAddrV4 {
    fn new(address: SocketAddrV4) -> Self {
        Self(Arc::new(AtomicU64::new(pack_address(address))))
    }

    fn load(&self) -> SocketAddr {
        SocketAddr::V4(unpack_address(self.0.load(Ordering::Acquire)))
    }

    fn store(&self, address: SocketAddrV4) {
        self.0.store(pack_address(address), Ordering::Release);
    }
}

struct RelayDestinations {
    phone_rtp: AtomicSocketAddrV4,
    phone_rtcp: AtomicSocketAddrV4,
    sip_rtp: AtomicSocketAddrV4,
    sip_rtcp: AtomicSocketAddrV4,
}

impl RelayDestinations {
    fn new(phone: SocketAddrV4, sip: SocketAddrV4) -> Self {
        Self {
            phone_rtp: AtomicSocketAddrV4::new(phone),
            phone_rtcp: AtomicSocketAddrV4::new(with_port_offset(phone, 1)),
            sip_rtp: AtomicSocketAddrV4::new(sip),
            sip_rtcp: AtomicSocketAddrV4::new(with_port_offset(sip, 1)),
        }
    }

    fn update(&self, phone: SocketAddrV4, sip: SocketAddrV4) {
        self.phone_rtp.store(phone);
        self.phone_rtcp.store(with_port_offset(phone, 1));
        self.sip_rtp.store(sip);
        self.sip_rtcp.store(with_port_offset(sip, 1));
    }
}

fn with_port_offset(address: SocketAddrV4, offset: u16) -> SocketAddrV4 {
    SocketAddrV4::new(*address.ip(), address.port().saturating_add(offset))
}

fn pack_address(address: SocketAddrV4) -> u64 {
    (u64::from(u32::from(*address.ip())) << 16) | u64::from(address.port())
}

fn unpack_address(value: u64) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::from((value >> 16) as u32), value as u16)
}

#[derive(Clone)]
struct Datagram {
    bytes: [u8; MAX_DATAGRAM],
    len: usize,
}

struct DirectionCounters {
    packets: Arc<AtomicU64>,
    drops: Arc<AtomicU64>,
}

fn spawn_direction(
    workers: &mut Vec<JoinHandle<()>>,
    name: &'static str,
    receiver: UdpSocket,
    sender: UdpSocket,
    destination: AtomicSocketAddrV4,
    stop: Arc<AtomicBool>,
    counters: DirectionCounters,
) -> io::Result<()> {
    let (mut producer, mut consumer) = RingBuffer::<Datagram>::new(QUEUE_PACKETS);
    let receive_stop = stop.clone();
    let receive_drops = counters.drops.clone();
    workers.push(
        thread::Builder::new()
            .name(format!("{name}-rx"))
            .spawn(move || {
                while !receive_stop.load(Ordering::Acquire) {
                    let mut packet = Datagram {
                        bytes: [0; MAX_DATAGRAM],
                        len: 0,
                    };
                    match receiver.recv(&mut packet.bytes) {
                        Ok(len) => {
                            packet.len = len;
                            if producer.push(packet).is_err() {
                                receive_drops.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => break,
                    }
                }
            })?,
    );

    workers.push(
        thread::Builder::new()
            .name(format!("{name}-tx"))
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match consumer.pop() {
                        Ok(packet) => {
                            if sender
                                .send_to(&packet.bytes[..packet.len], destination.load())
                                .is_ok()
                            {
                                counters.packets.fetch_add(1, Ordering::Relaxed);
                            } else {
                                counters.drops.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => thread::sleep(Duration::from_millis(1)),
                    }
                }
            })?,
    );
    Ok(())
}

pub struct RtpRelay {
    pub addresses: RelayAddresses,
    stats: RelayStats,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    destinations: RelayDestinations,
}

impl RtpRelay {
    pub fn stats(&self) -> RelayStatsSnapshot {
        self.stats.snapshot()
    }

    pub fn update_endpoints(&self, phone: SocketAddrV4, sip_peer: SocketAddrV4) {
        self.destinations.update(phone, sip_peer);
    }
}

impl Drop for RtpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_media_requires_a_matching_phone_to_sip_route() {
        let policy = MediaPolicy::new(vec![DirectMediaRoute {
            phones: vec!["10.20.0.0/16".parse().unwrap()],
            sip: vec!["192.168.40.0/24".parse().unwrap()],
        }]);
        assert_eq!(
            policy.select(
                "10.20.1.2".parse().unwrap(),
                "192.168.40.8".parse().unwrap(),
                true
            ),
            MediaMode::Direct
        );
        assert_eq!(
            policy.select(
                "10.20.1.2".parse().unwrap(),
                "192.168.1.2".parse().unwrap(),
                true
            ),
            MediaMode::Relay
        );
        assert_eq!(
            policy.select(
                "10.20.1.2".parse().unwrap(),
                "192.168.40.8".parse().unwrap(),
                false
            ),
            MediaMode::Relay
        );
        assert_eq!(
            policy.select(
                "192.168.40.8".parse().unwrap(),
                "10.20.1.2".parse().unwrap(),
                true
            ),
            MediaMode::Relay,
            "route sides are intentionally role-specific"
        );
    }

    #[test]
    fn direct_media_can_match_any_network_on_either_side() {
        let policy = MediaPolicy::new(vec![DirectMediaRoute {
            phones: vec![
                "10.10.0.0/16".parse().unwrap(),
                "10.20.0.0/16".parse().unwrap(),
            ],
            sip: vec![
                "172.16.1.0/24".parse().unwrap(),
                "172.16.2.0/24".parse().unwrap(),
            ],
        }]);
        assert_eq!(
            policy.select(
                "10.20.1.2".parse().unwrap(),
                "172.16.2.8".parse().unwrap(),
                true
            ),
            MediaMode::Direct
        );
        assert_eq!(
            MediaPolicy::default().select(
                "10.20.1.2".parse().unwrap(),
                "172.16.2.8".parse().unwrap(),
                true
            ),
            MediaMode::Relay
        );
    }

    #[test]
    fn relays_encoded_rtp_without_modifying_bytes() {
        // Let the kernel choose ports so the test remains isolated from other
        // test processes and host services.
        let phone_rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let phone_rtcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sip_rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sip_rtcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        for socket in [&phone_rtp, &phone_rtcp, &sip_rtp, &sip_rtcp] {
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
        }
        let allocated = AllocatedRelay {
            addresses: RelayAddresses {
                phone_facing_rtp: phone_rtp.local_addr().unwrap(),
                phone_facing_rtcp: phone_rtcp.local_addr().unwrap(),
                sip_facing_rtp: sip_rtp.local_addr().unwrap(),
                sip_facing_rtcp: sip_rtcp.local_addr().unwrap(),
            },
            phone_rtp,
            phone_rtcp,
            sip_rtp,
            sip_rtcp,
        };
        let addresses = allocated.addresses;
        let phone = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let sip = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        sip.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let relay = allocated
            .start(phone.local_addr().unwrap(), sip.local_addr().unwrap())
            .unwrap();
        let packet = [0x80, 0x00, 0, 1, 0, 0, 0, 160, 1, 2, 3, 4, 0xaa, 0xbb];
        phone.send_to(&packet, addresses.phone_facing_rtp).unwrap();
        let mut received = [0_u8; 64];
        let len = sip.recv(&mut received).unwrap();
        assert_eq!(&received[..len], &packet);

        let reverse_packet = [0x80, 0x08, 0, 2, 0, 0, 1, 64, 4, 3, 2, 1, 0xcc];
        sip.send_to(&reverse_packet, addresses.sip_facing_rtp)
            .unwrap();
        let len = phone.recv(&mut received).unwrap();
        assert_eq!(&received[..len], &reverse_packet);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(relay.stats().phone_to_sip_packets, 1);
        assert_eq!(relay.stats().sip_to_phone_packets, 1);
    }

    #[test]
    fn relays_rtcp_and_can_retarget_a_running_stream() {
        fn bind_pair() -> (UdpSocket, UdpSocket) {
            for _ in 0..100 {
                let rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
                let port = rtp.local_addr().unwrap().port();
                if let Some(rtcp_port) = port.checked_add(1)
                    && let Ok(rtcp) = UdpSocket::bind((Ipv4Addr::LOCALHOST, rtcp_port))
                {
                    return (rtp, rtcp);
                }
            }
            panic!("unable to reserve a consecutive UDP socket pair");
        }

        let (phone_facing_rtp, phone_facing_rtcp) = bind_pair();
        let (sip_facing_rtp, sip_facing_rtcp) = bind_pair();
        for socket in [
            &phone_facing_rtp,
            &phone_facing_rtcp,
            &sip_facing_rtp,
            &sip_facing_rtcp,
        ] {
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
        }
        let allocated = AllocatedRelay {
            addresses: RelayAddresses {
                phone_facing_rtp: phone_facing_rtp.local_addr().unwrap(),
                phone_facing_rtcp: phone_facing_rtcp.local_addr().unwrap(),
                sip_facing_rtp: sip_facing_rtp.local_addr().unwrap(),
                sip_facing_rtcp: sip_facing_rtcp.local_addr().unwrap(),
            },
            phone_rtp: phone_facing_rtp,
            phone_rtcp: phone_facing_rtcp,
            sip_rtp: sip_facing_rtp,
            sip_rtcp: sip_facing_rtcp,
        };
        let addresses = allocated.addresses;
        let (phone_rtp, phone_rtcp) = bind_pair();
        let (sip_rtp, sip_rtcp) = bind_pair();
        sip_rtp
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        sip_rtcp
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let relay = allocated
            .start(
                phone_rtp.local_addr().unwrap(),
                sip_rtp.local_addr().unwrap(),
            )
            .unwrap();

        let report = [0x80, 0xc9, 0, 1, 1, 2, 3, 4];
        phone_rtcp
            .send_to(&report, addresses.phone_facing_rtcp)
            .unwrap();
        let mut received = [0_u8; 64];
        let len = sip_rtcp.recv(&mut received).unwrap();
        assert_eq!(&received[..len], &report);

        let (new_sip_rtp, _new_sip_rtcp) = bind_pair();
        new_sip_rtp
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        relay.update_endpoints(
            match phone_rtp.local_addr().unwrap() {
                SocketAddr::V4(address) => address,
                SocketAddr::V6(_) => unreachable!(),
            },
            match new_sip_rtp.local_addr().unwrap() {
                SocketAddr::V4(address) => address,
                SocketAddr::V6(_) => unreachable!(),
            },
        );
        let rtp_packet = [0x80, 0, 0, 3, 4, 3, 2, 1, 9, 8, 7, 6, 0xaa];
        phone_rtp
            .send_to(&rtp_packet, addresses.phone_facing_rtp)
            .unwrap();
        let len = new_sip_rtp.recv(&mut received).unwrap();
        assert_eq!(&received[..len], &rtp_packet);
    }

    #[test]
    fn packed_destinations_round_trip_and_update() {
        let first: SocketAddrV4 = "10.1.2.3:16384".parse().unwrap();
        let second: SocketAddrV4 = "192.168.40.8:30000".parse().unwrap();
        assert_eq!(unpack_address(pack_address(first)), first);
        let destination = AtomicSocketAddrV4::new(first);
        assert_eq!(destination.load(), SocketAddr::V4(first));
        destination.store(second);
        assert_eq!(destination.load(), SocketAddr::V4(second));
    }
}
