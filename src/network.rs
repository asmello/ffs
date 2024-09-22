use sha2::{Digest, Sha256};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    net::UdpSocket,
};

use crate::protocol::Hash;

pub mod client;
pub mod server;

const IPV6_MULTICAST_ADDR: Ipv6Addr = Ipv6Addr::new(0xff08, 0, 0, 0, 0, 0, 0xda, 0xda);
const IPV4_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 242);
const MULTICAST_LISTEN_PORT: u16 = 43549;

struct UnicastInterface {
    socket: UdpSocket,
    mcast_addr: SocketAddr,
}

fn domain_of(addr: SocketAddr) -> Domain {
    if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    }
}

fn setup_unicast(addr: SocketAddr) -> io::Result<UnicastInterface> {
    let unicast = Socket::new(domain_of(addr), Type::DGRAM, Some(Protocol::UDP))?;
    unicast.set_nonblocking(true)?;
    unicast.bind(&SockAddr::from(addr))?;
    Ok(UnicastInterface {
        socket: UdpSocket::from_std(unicast.into())?,
        mcast_addr: match addr.ip() {
            IpAddr::V4(_) => SocketAddr::new(IPV4_MULTICAST_ADDR.into(), MULTICAST_LISTEN_PORT),
            IpAddr::V6(_) => SocketAddr::new(IPV6_MULTICAST_ADDR.into(), MULTICAST_LISTEN_PORT),
        },
    })
}

struct MulticastInterface {
    mcast_sock: UdpSocket,
    ucast_sock: UdpSocket,
}

fn setup_multicast(unicast_addr: SocketAddr) -> io::Result<MulticastInterface> {
    let UnicastInterface {
        socket: unicast, ..
    } = setup_unicast(unicast_addr)?;

    let multicast = {
        let multicast = Socket::new(domain_of(unicast_addr), Type::DGRAM, Some(Protocol::UDP))?;
        multicast.set_nonblocking(true)?;
        match unicast_addr.ip() {
            IpAddr::V4(addr) => {
                multicast.join_multicast_v4(&IPV4_MULTICAST_ADDR, &addr)?;
            }
            IpAddr::V6(_) => {
                // TODO: may be necessary to set interface explicitly in MacOS
                multicast.join_multicast_v6(&IPV6_MULTICAST_ADDR, 0)?;
                multicast.set_only_v6(true)?;
            }
        }
        // may seem strange to bind to the *unicast* ip here, but all this
        // influences is which interface the socket is bound to, really. we wish
        // to use the same interface we use for unicast communication, so we derive
        // the interface from the unicast ip. the port is derived from the multicast
        // group, though.
        multicast.bind(&SocketAddr::new(unicast_addr.ip(), MULTICAST_LISTEN_PORT).into())?;
        UdpSocket::from_std(multicast.into())?
    };

    Ok(MulticastInterface {
        ucast_sock: unicast,
        mcast_sock: multicast,
    })
}

// set to tokio's default `max_buf_size`
const HASHING_CHUNK_SIZE: usize = 2 * 1024 * 1024;

// TODO: feels like async is not a great fit here
async fn hash(mut file: File, size: usize, mut buf: &mut Vec<u8>) -> io::Result<Hash<'static>> {
    file.rewind().await?;
    let mut hasher = Sha256::new();
    let mut bytes_read = 0;
    while bytes_read < size {
        buf.clear();
        let n = file.read_buf(&mut buf).await?;
        hasher.update(&*buf);
        bytes_read += n;
    }
    file.rewind().await?;
    Ok(hasher
        .finalize()
        .to_vec()
        .try_into()
        .expect("sha256 has 32 bytes exactly"))
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
