use super::{addresses, create_socket, Hash, IPVersion, SocketMode};
use crate::protocol::{ClientMessage, ServerMessage};
use rand::Rng;
use std::{collections::HashMap, io};
use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt},
};

struct FileEntry {
    file: File,
    hash: Hash,
    curr_size: usize,
    expected_size: usize,
}

pub async fn serve(name: &str, ip_version: IPVersion, overwrite: bool) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let listen_socket = create_socket(addr.recv, SocketMode::Receive)?;
    let send_socket = create_socket(addr.send, SocketMode::Send)?;
    let local_addr = listen_socket.local_addr().unwrap();
    let mut open_opts = tokio::fs::OpenOptions::new();
    open_opts.write(true).read(false);
    if overwrite {
        open_opts.create(true).truncate(true);
    } else {
        open_opts.create_new(true);
    }

    let mut sessions = HashMap::new();
    let mut buf = Vec::with_capacity(65535);

    tracing::info!("listening on {local_addr:?}");

    loop {
        buf.clear();
        let (_, addr) = listen_socket.recv_buf_from(&mut buf).await?;
        let Ok(msg) = bitcode::decode(&buf) else {
            tracing::error!(?buf, "invalid message received");
            continue;
        };

        macro_rules! try_reply {
            ($msg:expr) => {
                if let Err(err) = send_socket.send_to(&bitcode::encode($msg), addr).await {
                    tracing::error!(?err, "failed to send message");
                    continue;
                }
            };
        }

        match msg {
            ClientMessage::Discover => {
                let announce_msg = ServerMessage::Announce(name);
                let payload = bitcode::encode(&announce_msg);
                send_socket.send_to(&payload, addr).await?;
            }
            ClientMessage::Start {
                nonce,
                size,
                hash,
                path,
            } => {
                let file = match open_opts.open(path).await {
                    Ok(file) => file,
                    Err(err) => {
                        tracing::error!("failed to open file: {path}");
                        try_reply!(&ServerMessage::Nack {
                            nonce,
                            msg: &format!("failed to open file: {err}"),
                        });
                        continue;
                    }
                };
                // TODO: can we skip this? docs say seek past end is UB...
                if let Err(err) = file.set_len(size).await {
                    tracing::error!(?err, "could not pre-allocate file");
                    try_reply!(&ServerMessage::Nack {
                        nonce,
                        msg: &format!("failed to pre-allocate file: {err}"),
                    });
                    continue;
                }
                let id = rand::thread_rng().gen();
                try_reply!(&ServerMessage::Ack { nonce, id });
                sessions.insert(
                    id,
                    FileEntry {
                        file,
                        hash,
                        curr_size: 0,
                        expected_size: size as usize,
                    },
                );
            }
            ClientMessage::Data { id, chunk, content } => {
                let Some(entry) = sessions.get_mut(&id) else {
                    tracing::trace!("ignoring unknown session {id:#08x}");
                    continue;
                };
                if let Err(err) = entry.file.seek(io::SeekFrom::Start(chunk)).await {
                    tracing::error!(?err, "could not seek file, aborting session");
                    sessions.remove(&id);
                    try_reply!(&ServerMessage::Error {
                        id,
                        msg: &format!("at chunk {chunk}, error seeking: {err}")
                    });
                    continue;
                }
                if let Err(err) = entry.file.write_all_buf(&mut content.as_slice()).await {
                    tracing::error!(?err, "could not write chunk, aborting session");
                    sessions.remove(&id);
                    try_reply!(&ServerMessage::Error {
                        id,
                        msg: &format!("at chunk {chunk}, error writing: {err}")
                    });
                    continue;
                }
                entry.curr_size += content.len();
                if entry.curr_size == entry.expected_size {
                    // TODO: check hash
                    sessions.remove(&id);
                    try_reply!(&ServerMessage::Done { id });
                }
                // TODO: check missing chunks, ask for resends
            }
        }
    }
}
