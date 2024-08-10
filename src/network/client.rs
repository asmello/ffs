use crate::protocol::Message;

use super::{
    addresses, create_socket, Addresses, IPVersion, SocketMode, IPV4_MULTICAST_ADDR,
    IPV6_MULTICAST_ADDR, LISTEN_PORT,
};
use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
};

pub async fn send(ip_version: IPVersion, _path: &Path) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let socket = create_socket(addr.send, SocketMode::Send)?;

    let discover_msg = bitcode::encode(&Message::Discover);
    socket.send_to(&discover_msg, addr.recv).await?;

    let mut buf = Vec::with_capacity(65536);

    loop {
        let (_, addr) = socket.recv_buf_from(&mut buf).await?;
        let msg: Message = bitcode::decode(&buf)?;
        match msg {
            Message::Discover => {
                tracing::error!("client received discover message");
            }
            Message::Announce(name) => {
                tracing::info!(%addr, "discovered server: {name}");
            }
            Message::Start {
                nonce,
                size,
                hash,
                path,
            } => todo!(),
            Message::Ack { nonce, id } => todo!(),
            Message::Nack { nonce, msg } => todo!(),
            Message::Data { id, chunk, content } => todo!(),
            Message::Error { id, msg } => todo!(),
            Message::Repeat { id, chunks } => todo!(),
            Message::Done { id } => todo!(),
        }
        buf.clear();
    }
}
