use super::{addresses, create_socket, IpVersion, SocketMode};
use crate::protocol::{ClientMessage, Hash, Identifier, ServerMessage, DATAGRAM_SIZE_LIMIT};
use rand::Rng;
use std::{cmp::Ordering, collections::HashMap, io, net::SocketAddr, path::Path, time::Duration};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
    net::UdpSocket,
};
use tracing::Level;

struct FileEntry {
    file: File,
    hash: Hash<'static>,
    curr_size: usize,
    expected_size: usize,
}

pub async fn serve(name: &str, ip_version: IpVersion, overwrite: bool) -> eyre::Result<()> {
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
    let mut read_buf = Vec::with_capacity(65535);
    let mut write_buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

    tracing::info!("listening on {local_addr:?}");

    loop {
        read_buf.clear();
        tokio::select! {
            r = listen_socket.recv_buf_from(&mut read_buf) => {
                let (_, addr) = r?;
                let Ok(msg) = ClientMessage::decode(&read_buf) else {
                    tracing::error!(?read_buf, "invalid message received");
                    continue;
                };
                handle_message(
                    addr,
                    msg,
                    &mut write_buf,
                    &send_socket,
                    name,
                    &mut sessions,
                    &open_opts
                ).await?;
            }
            // if we have active sessions but go 5 seconds without receiving any
            // messages, odds are we got a problem
            _ = tokio::time::sleep(Duration::from_secs(5)),
                if tracing::enabled!(Level::DEBUG) && !sessions.is_empty() => {
                tracing::debug!(
                    ids = ?sessions.keys().collect::<Vec<_>>(),
                    "no progress in the last 5 seconds, we might be stuck"
                );
            }
        }
    }
}

async fn handle_message(
    src: SocketAddr,
    msg: ClientMessage<'_>,
    mut buf: &mut Vec<u8>,
    socket: &UdpSocket,
    name: &str,
    sessions: &mut HashMap<Identifier, FileEntry>,
    open_opts: &OpenOptions,
) -> eyre::Result<()> {
    macro_rules! try_reply {
        ($msg:expr) => {
            buf.clear();
            $msg.encode(&mut buf).expect("vec grows as needed");
            if let Err(err) = socket.send_to(&buf, src).await {
                tracing::error!(?err, "failed to send message");
                return Ok(());
            }
        };
    }

    match msg {
        ClientMessage::Discover => {
            try_reply!(ServerMessage::Announce(name));
        }
        ClientMessage::Start {
            nonce,
            size,
            hash,
            path,
        } => {
            let path = Path::new(path);
            // let's make sure the ancestor directories exist...
            // TODO: make sure there aren't any `..` in the path
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let file = match open_opts.open(path).await {
                Ok(file) => file,
                Err(err) => {
                    tracing::error!("failed to open file: {}", path.display());
                    try_reply!(ServerMessage::Nack {
                        nonce,
                        msg: &format!("failed to open file: {err}"),
                    });
                    return Ok(());
                }
            };
            // TODO: can we skip this? docs say seek past end is UB...
            if let Err(err) = file.set_len(size).await {
                tracing::error!(?err, "could not pre-allocate file");
                try_reply!(ServerMessage::Nack {
                    nonce,
                    msg: &format!("failed to pre-allocate file: {err}"),
                });
                return Ok(());
            }
            let id = rand::thread_rng().gen();
            try_reply!(ServerMessage::Ack { nonce, id });
            tracing::info!(%id, path = %path.display(), %hash, size, "started a new file transfer session");
            sessions.insert(
                id,
                FileEntry {
                    file,
                    hash: hash.into_owned(),
                    curr_size: 0,
                    expected_size: size as usize,
                },
            );
        }
        ClientMessage::Data {
            id,
            offset,
            mut content,
        } => {
            let Some(entry) = sessions.get_mut(&id) else {
                tracing::trace!(%id, offset, len = content.len(), "ignoring chunk for unknown session");
                return Ok(());
            };
            tracing::debug!(
                %id,
                offset,
                len = content.len(),
                rem = entry.expected_size - entry.curr_size - content.len(),
                "received a new chunk"
            );
            if let Err(err) = entry.file.seek(io::SeekFrom::Start(offset)).await {
                tracing::error!(?err, "could not seek file, aborting session");
                sessions.remove(&id);
                try_reply!(ServerMessage::Error {
                    id,
                    msg: &format!("at chunk {offset}, error seeking: {err}")
                });
                return Ok(());
            }
            let len = content.len();
            if let Err(err) = entry.file.write_all_buf(&mut content).await {
                tracing::error!(?err, "could not write chunk, aborting session");
                sessions.remove(&id);
                try_reply!(ServerMessage::Error {
                    id,
                    msg: &format!("at chunk {offset}, error writing: {err}")
                });
                return Ok(());
            }
            tracing::trace!(%id, offset, "wrote chunk successfully");
            entry.curr_size += len;
            match entry.curr_size.cmp(&entry.expected_size) {
                Ordering::Less => {
                    tracing::trace!("remaining bytes: {}", entry.expected_size - entry.curr_size);
                }
                Ordering::Equal => {
                    // TODO: check hash
                    let entry = sessions
                        .remove(&id)
                        .expect("we hold a &mut so the entry must still be there");
                    try_reply!(ServerMessage::Done { id });
                    tracing::info!(%id, len = entry.curr_size, hash = %entry.hash, "file transfer completed");
                }
                Ordering::Greater => {
                    tracing::error!(
                        recv = entry.curr_size,
                        expected = entry.expected_size,
                        "received more data than expected"
                    );
                    let entry = sessions
                        .remove(&id)
                        .expect("we hold a &mut so the entry must still be there");
                    try_reply!(ServerMessage::Error {
                        id,
                        msg: &format!(
                            "payload overflow: received {} bytes, expected {}",
                            entry.curr_size, entry.expected_size
                        )
                    });
                }
            }
            // TODO: check missing chunks, ask for resends
        }
    }

    Ok(())
}
