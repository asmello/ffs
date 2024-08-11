use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};
use tokio::net::UdpSocket;

pub mod client;
pub mod server;

const IPV6_MULTICAST_ADDR: Ipv6Addr = Ipv6Addr::new(0xff08, 0, 0, 0, 0, 0, 0xda, 0xda);
const IPV4_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 242);
const LISTEN_PORT: u16 = 43549;

pub enum IPVersion {
    IPV4,
    IPV6,
}

enum SocketMode {
    Send,
    Receive,
}

fn create_socket(addr: SocketAddr, mode: SocketMode) -> io::Result<UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };

    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;

    if matches!(mode, SocketMode::Receive) {
        match addr.ip() {
            IpAddr::V4(mc_addr) => {
                socket.join_multicast_v4(&mc_addr, &Ipv4Addr::UNSPECIFIED)?;
            }
            IpAddr::V6(mc_addr) => {
                // TODO: may be necessary to set interface explicitly in MacOS
                socket.join_multicast_v6(&mc_addr, 0)?;
                socket.set_only_v6(true)?;
            }
        }
    }

    socket.bind(&SockAddr::from(addr))?;

    UdpSocket::from_std(socket.into())
}

struct Addresses {
    send: SocketAddr,
    recv: SocketAddr,
}

fn addresses(ip_version: IPVersion) -> Addresses {
    match ip_version {
        IPVersion::IPV4 => Addresses {
            send: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
            recv: SocketAddr::new(IPV4_MULTICAST_ADDR.into(), LISTEN_PORT),
        },
        IPVersion::IPV6 => Addresses {
            send: SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0),
            recv: SocketAddr::new(IPV6_MULTICAST_ADDR.into(), LISTEN_PORT),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_multicast() {
        assert!(IPV4_MULTICAST_ADDR.is_multicast());
        assert!(IPV6_MULTICAST_ADDR.is_multicast());
    }
}
