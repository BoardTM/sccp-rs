//! IP socket marking independent of the protocol layered over the socket.
//!
//! Policies can be applied to either a captured signaling socket or a borrowed
//! media socket without transferring ownership of the underlying descriptor.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsSocket;

use socket2::{SockRef, Socket};

use crate::types::SignalingQos;

/// A socket option that could not be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketQosMark {
    Dscp,
    SocketPriority,
}

/// One failed marking operation.
#[derive(Debug)]
pub struct SocketQosFailure {
    mark: SocketQosMark,
    source: io::Error,
}

impl SocketQosFailure {
    pub const fn mark(&self) -> SocketQosMark {
        self.mark
    }

    pub const fn source(&self) -> &io::Error {
        &self.source
    }
}

impl fmt::Display for SocketQosFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mark = match self.mark {
            SocketQosMark::Dscp => "DSCP",
            SocketQosMark::SocketPriority => "socket priority",
        };
        write!(formatter, "unable to apply socket {mark}: {}", self.source)
    }
}

/// Independent results from applying every supported socket mark.
#[derive(Debug, Default)]
pub struct SocketQosReport {
    failures: Vec<SocketQosFailure>,
}

impl SocketQosReport {
    pub fn failed(mark: SocketQosMark, source: io::Error) -> Self {
        Self {
            failures: vec![SocketQosFailure { mark, source }],
        }
    }

    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failures(&self) -> impl ExactSizeIterator<Item = &SocketQosFailure> {
        self.failures.iter()
    }

    /// Consume the report and return each independent marking failure.
    pub fn into_failures(self) -> impl ExactSizeIterator<Item = SocketQosFailure> {
        self.failures.into_iter()
    }

    fn push(&mut self, mark: SocketQosMark, source: io::Error) {
        self.failures.push(SocketQosFailure { mark, source });
    }
}

/// Typed socket-marking values accepted by the shared platform adapter.
pub trait SocketQosPolicy: Copy {
    /// Six-bit differentiated-services code point.
    fn dscp(self) -> u8;

    /// Three-bit class-of-service priority.
    fn cos(self) -> u8;
}

impl SocketQosPolicy for SignalingQos {
    fn dscp(self) -> u8 {
        self.dscp
    }

    fn cos(self) -> u8 {
        self.cos
    }
}

/// Session-owned capability for changing the underlying signaling socket.
///
/// Implementations attempt DSCP and socket priority independently. A partial
/// failure is returned in the report and must not terminate the station
/// session.
pub trait StationSocketQos: fmt::Debug + Send + Sync {
    fn apply(&self, qos: SignalingQos) -> SocketQosReport;
}

/// Clone of a TCP socket retained independently of its clear or TLS stream.
#[derive(Debug)]
pub struct SignalingSocket {
    socket: Socket,
    local: SocketAddr,
}

impl SignalingSocket {
    #[cfg(unix)]
    pub fn capture<S>(socket: &S, local: SocketAddr) -> io::Result<Self>
    where
        S: AsFd,
    {
        Ok(Self {
            socket: SockRef::from(socket).try_clone()?,
            local,
        })
    }

    #[cfg(windows)]
    pub fn capture<S>(socket: &S, local: SocketAddr) -> io::Result<Self>
    where
        S: AsSocket,
    {
        Ok(Self {
            socket: SockRef::from(socket).try_clone()?,
            local,
        })
    }
}

impl StationSocketQos for SignalingSocket {
    fn apply(&self, qos: SignalingQos) -> SocketQosReport {
        apply_socket_marks(&self.socket, self.local.ip(), qos)
    }
}

/// Apply typed marking policy to a borrowed socket without taking ownership.
///
/// This is suitable for sockets owned by a larger media or signaling object.
/// The descriptor remains open and owned by its original RAII guard.
#[cfg(unix)]
pub fn apply_socket_qos<S, Q>(socket: &S, qos: Q) -> io::Result<SocketQosReport>
where
    S: AsFd,
    Q: SocketQosPolicy,
{
    apply_borrowed_socket_qos(SockRef::from(socket), qos)
}

/// Windows form of [`apply_socket_qos`].
#[cfg(windows)]
pub fn apply_socket_qos<S, Q>(socket: &S, qos: Q) -> io::Result<SocketQosReport>
where
    S: AsSocket,
    Q: SocketQosPolicy,
{
    apply_borrowed_socket_qos(SockRef::from(socket), qos)
}

fn apply_borrowed_socket_qos(
    socket: SockRef<'_>,
    qos: impl SocketQosPolicy,
) -> io::Result<SocketQosReport> {
    let local = socket.local_addr()?.as_socket().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "socket address family does not support IP QoS",
        )
    })?;
    Ok(apply_socket_marks(&socket, local.ip(), qos))
}

fn apply_socket_marks(
    socket: &Socket,
    local_address: IpAddr,
    qos: impl SocketQosPolicy,
) -> SocketQosReport {
    let mut report = SocketQosReport::default();
    let dscp = qos.dscp();
    let dscp_result = if dscp <= 63 {
        apply_dscp(socket, local_address, dscp)
    } else {
        Err(invalid_qos_value("DSCP", dscp, 63))
    };
    if let Err(source) = dscp_result {
        report.push(SocketQosMark::Dscp, source);
    }
    let cos = qos.cos();
    let priority_result = if cos <= 7 {
        apply_socket_priority(socket, cos)
    } else {
        Err(invalid_qos_value("COS", cos, 7))
    };
    if let Err(source) = priority_result {
        report.push(SocketQosMark::SocketPriority, source);
    }
    report
}

fn invalid_qos_value(name: &str, value: u8, maximum: u8) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{name} {value} exceeds {maximum}"),
    )
}

fn apply_dscp(socket: &Socket, address: IpAddr, dscp: u8) -> io::Result<()> {
    let traffic_class = u32::from(dscp) << 2;
    match address {
        IpAddr::V4(_) => apply_ipv4_traffic_class(socket, traffic_class),
        IpAddr::V6(_) => apply_ipv6_traffic_class(socket, traffic_class),
    }
}

#[cfg(not(any(
    target_os = "fuchsia",
    target_os = "redox",
    target_os = "solaris",
    target_os = "haiku",
    target_os = "wasi",
)))]
fn apply_ipv4_traffic_class(socket: &Socket, traffic_class: u32) -> io::Result<()> {
    socket.set_tos_v4(traffic_class)
}

#[cfg(any(
    target_os = "fuchsia",
    target_os = "redox",
    target_os = "solaris",
    target_os = "haiku",
    target_os = "wasi",
))]
fn apply_ipv4_traffic_class(_socket: &Socket, _traffic_class: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "IPv4 DSCP marking is unavailable on this platform",
    ))
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "cygwin",
    target_os = "illumos",
))]
fn apply_ipv6_traffic_class(socket: &Socket, traffic_class: u32) -> io::Result<()> {
    socket.set_tclass_v6(traffic_class)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "cygwin",
    target_os = "illumos",
)))]
fn apply_ipv6_traffic_class(_socket: &Socket, _traffic_class: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "IPv6 DSCP marking is unavailable on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
fn apply_socket_priority(socket: &Socket, cos: u8) -> io::Result<()> {
    socket.set_priority(u32::from(cos))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "fuchsia")))]
fn apply_socket_priority(_socket: &Socket, _cos: u8) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "socket priority marking is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, TcpListener, UdpSocket};

    use super::*;

    #[test]
    fn dscp_is_shifted_into_the_ipv4_traffic_class() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let socket = SignalingSocket::capture(&listener, listener.local_addr().unwrap()).unwrap();

        let report = socket.apply(SignalingQos::new(26, 0));

        assert!(
            report
                .failures()
                .all(|failure| failure.mark() == SocketQosMark::SocketPriority)
        );
        assert_eq!(socket.socket.tos_v4().unwrap(), 26 << 2);
    }

    #[test]
    fn borrowed_udp_sockets_are_marked_independently_without_ownership_transfer() {
        let rtp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let rtcp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();

        let rtp_report = apply_socket_qos(&rtp, SignalingQos::new(46, 6)).unwrap();
        let rtcp_report = apply_socket_qos(&rtcp, SignalingQos::new(46, 6)).unwrap();

        assert!(rtp_report.failures().all(platform_priority_failure));
        assert!(rtcp_report.failures().all(platform_priority_failure));
        assert_eq!(SockRef::from(&rtp).tos_v4().unwrap(), 46 << 2);
        assert_eq!(SockRef::from(&rtcp).tos_v4().unwrap(), 46 << 2);
        assert!(rtp.local_addr().is_ok());
        assert!(rtcp.local_addr().is_ok());
    }

    #[test]
    fn public_adapter_rejects_out_of_range_marks_independently() {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();

        let report = apply_socket_qos(&socket, SignalingQos::new(64, 8)).unwrap();
        let failures = report.failures().collect::<Vec<_>>();

        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0].mark(), SocketQosMark::Dscp);
        assert_eq!(failures[0].source().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(failures[1].mark(), SocketQosMark::SocketPriority);
        assert_eq!(failures[1].source().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(SockRef::from(&socket).tos_v4().unwrap(), 0);
        assert!(socket.local_addr().is_ok());
    }

    fn platform_priority_failure(failure: &SocketQosFailure) -> bool {
        failure.mark() == SocketQosMark::SocketPriority
            && failure.source().kind() == io::ErrorKind::Unsupported
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "fuchsia"))]
    #[test]
    fn cos_is_applied_as_socket_priority_when_supported() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let socket = SignalingSocket::capture(&listener, listener.local_addr().unwrap()).unwrap();

        assert!(socket.apply(SignalingQos::new(0, 5)).is_complete());
        assert_eq!(socket.socket.priority().unwrap(), 5);
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "fuchsia")))]
    #[test]
    fn unsupported_socket_priority_is_reported_after_dscp_succeeds() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let socket = SignalingSocket::capture(&listener, listener.local_addr().unwrap()).unwrap();

        let report = socket.apply(SignalingQos::new(46, 6));

        let failure = report.failures().next().unwrap();
        assert_eq!(failure.mark(), SocketQosMark::SocketPriority);
        assert_eq!(failure.source().kind(), io::ErrorKind::Unsupported);
        assert_eq!(socket.socket.tos_v4().unwrap(), 46 << 2);
    }
}
