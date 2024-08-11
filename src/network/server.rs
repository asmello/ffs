use super::{addresses, create_socket, IPVersion, SocketMode};
use crate::protocol::Message;

pub async fn serve(name: &str, ip_version: IPVersion) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let listen_socket = create_socket(addr.recv, SocketMode::Receive)?;
    let send_socket = create_socket(addr.send, SocketMode::Send)?;
    let local_addr = listen_socket.local_addr().unwrap();
    tracing::info!("listening on {local_addr:?}");

    let mut buf = Vec::with_capacity(65535);

    loop {
        let (_, addr) = listen_socket.recv_buf_from(&mut buf).await?;
        let Ok(msg) = bitcode::decode(&buf) else {
            tracing::error!(?buf, "invalid message received");
            continue;
        };
        match msg {
            Message::Discover => {
                let announce_msg = Message::Announce(name);
                let payload = bitcode::encode(&announce_msg);
                send_socket.send_to(&payload, addr).await?;
            }
            Message::Announce(_) => todo!(),
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
