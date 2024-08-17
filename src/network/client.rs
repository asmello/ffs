use super::{addresses, create_socket, IpVersion, SocketMode};
use crate::{
    file_generator::FileGenerator,
    protocol::{ClientMessage, Hash, Identifier, Nonce, ServerMessage},
    tui::Tui,
};
use crossterm::event::{Event, KeyCode};
use rand::Rng;
use ratatui::{backend::CrosstermBackend, Terminal};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::HashMap,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    net::UdpSocket,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};
use tokio_stream::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use tracing::Level;

// assuming MTU of 1500 (typical for ethernet), this is 1500 - 40 (ipv6 header)
// - 8 (udp header) = 1452. if we estimate this too high, most networks will
// just fragment the packet, which is not the end of the world, but may increase
// re-send rate (as even one fragment lost invalidates the entire datagram).
const DATAGRAM_SIZE_LIMIT: usize = 1452;

pub async fn send_to_all(ip_version: IpVersion, path: &Path) -> eyre::Result<()> {
    let mut tasks = Vec::new();
    let mut task_rx = {
        let addr = addresses(ip_version);
        let socket = create_socket(addr.send, SocketMode::Send)?;
        let (task_tx, task_rx) = mpsc::unbounded_channel();
        // TODO: non-empty stream!
        let handle = tokio::spawn(dispatch_server_msgs(
            socket,
            addr.recv,
            FileGenerator::new(path).into_stream(),
            task_tx,
        ));
        tasks.push((handle, "dispatch_server_msgs".into()));
        task_rx
    };

    while let Some((handle, ref name)) = tasks.last_mut() {
        tokio::select! {
            // NOTE: this arm will be disabled when the channel closes
            Some(task) = task_rx.recv() => {
                tasks.push(task);
            }
            r = handle => {
                match r {
                    Ok(Ok(())) => {
                        tracing::debug!("task {name} completed successfully");
                    }
                    Ok(Err(err)) => {
                        tracing::error!(?err, "task {name} terminated unsuccesfully");
                    }
                    Err(join_err) => {
                        if join_err.is_cancelled() {
                            continue;
                        }
                        std::panic::resume_unwind(join_err.into_panic());
                    }
                }
                tasks.pop();
            }
        }
    }

    Ok(())
}

// NOTE: this task must be the exclusive reader of the socket
async fn dispatch_server_msgs(
    socket: UdpSocket,
    bcast_addr: SocketAddr,
    paths: impl Stream<Item = io::Result<PathBuf>>,
    task_sender: mpsc::UnboundedSender<(JoinHandle<eyre::Result<()>>, String)>,
) -> eyre::Result<()> {
    let socket = Arc::new(socket);
    let mut error_count = 0;
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);
    let mut pending_sessions = HashMap::new();
    let mut sessions = HashMap::new();
    tokio::pin!(paths);
    loop {
        buf.clear();
        tokio::select! {
            // NOTE: arm will be *disabled* when paths is exhausted
            Some(r) = paths.next() => {
                let path = match r {
                    Ok(path) => path,
                    Err(err) => {
                        tracing::error!(?err, "filesystem io error");
                        error_count += 1;
                        continue;
                    }
                };
                let (nonce, size) = start_session(&path, &socket, bcast_addr).await?;
                pending_sessions.insert(nonce, (path, size));
            }
            r = socket.recv_buf_from(&mut buf) => {
                let (_, src) = r?;
                let Ok(msg) = bitcode::decode::<ServerMessage>(&buf) else {
                    tracing::error!(?buf, "received invalid message");
                    continue;
                };
                handle_msg(src, msg, &mut pending_sessions, &mut sessions, &socket, bcast_addr, &task_sender).await?;
                // TODO: grace period so we wait for all servers to ack
                if sessions.is_empty() {
                    tracing::info!("completed all sessions!");
                    break;
                }
            }
        }
    }

    eyre::ensure!(error_count == 0, "{error_count} file(s) could not be sent");

    Ok(())
}

async fn start_session(
    path: &Path,
    socket: &UdpSocket,
    bcast_addr: SocketAddr,
) -> eyre::Result<(Nonce, u64)> {
    tracing::info!(path = %path.display(), %bcast_addr, "starting a broadcast file transfer");

    let nonce = rand::thread_rng().gen();
    let mut buf;

    let mut file = tokio::fs::File::open(&path).await?;
    let size = {
        let meta = file.metadata().await?;
        meta.len()
    };

    // set to tokio's default `max_buf_size`
    const CHUNK_SIZE: u64 = 2 * 1024 * 1024;
    buf = Vec::with_capacity(size.min(CHUNK_SIZE) as usize);

    let hash: Hash = {
        // scan the file once to compute its hash
        // TODO: is there a better way?
        let mut hasher = Sha256::new();
        let mut bytes_read = 0;
        loop {
            let n = file.read_buf(&mut buf).await?;
            hasher.update(&buf);
            bytes_read += n;
            if bytes_read >= size as usize {
                break;
            }
            buf.clear();
        }
        file.rewind().await?;
        hasher
            .finalize()
            .to_vec()
            .try_into()
            .expect("sha256 has 32 bytes exactly")
    };

    // send start message
    let start_msg = {
        let path = path
            .as_os_str()
            .to_str()
            .ok_or_else(|| eyre::eyre!("not a valid utf-8 path: {path:?}"))?;

        tracing::debug!(%nonce, %path, size, %hash, "sending start message");
        let msg = ClientMessage::Start {
            nonce,
            size,
            hash,
            path,
        };

        bitcode::encode(&msg)
    };

    socket.send_to(&start_msg, bcast_addr).await?;

    Ok((nonce, size))
}

async fn handle_msg(
    src: SocketAddr,
    msg: ServerMessage<'_>,
    pending: &mut HashMap<Nonce, (PathBuf, u64)>,
    sessions: &mut HashMap<Identifier, (Arc<PathBuf>, u64)>,
    socket: &Arc<UdpSocket>,
    bcast_addr: SocketAddr,
    task_sender: &mpsc::UnboundedSender<(JoinHandle<eyre::Result<()>>, String)>,
) -> eyre::Result<()> {
    macro_rules! try_send {
        ($val:expr) => {
            if task_sender.send($val).is_err() {
                eyre::bail!("handles channel closed, main task terminated prematurely");
            }
        };
    }

    match msg {
        ServerMessage::Ack { nonce, id } => {
            if let Some((path, size)) = pending.remove(&nonce) {
                tracing::info!(
                    %nonce,
                    %id,
                    "file transfer session started"
                );
                let path = Arc::new(path);
                let handle = tokio::spawn(send_all_chunks(
                    Arc::clone(&path),
                    size,
                    Arc::clone(socket),
                    id,
                    bcast_addr,
                ));
                sessions.insert(id, (path, size));
                try_send!((handle, format!("send_all_chunks(id={id})")));
            } else {
                tracing::debug!(%nonce, "ignoring ack with unknown nonce");
            }
        }
        ServerMessage::Nack { nonce, msg } => {
            if pending.remove(&nonce).is_some() {
                tracing::error!(msg, "server rejected file transfer");
            } else {
                tracing::debug!("ignoring nack with unknown nonce");
            }
        }
        ServerMessage::Error { id, msg } => {
            if sessions.remove(&id).is_some() {
                tracing::error!(%id, msg, "remote server error, session terminated");
            } else {
                tracing::debug!(%id, msg, "ignoring server error for unknown session");
            }
        }
        ServerMessage::Repeat { id, offsets } => {
            if let Some((path, size)) = sessions.get(&id) {
                tracing::debug!(
                    %id,
                    ?offsets,
                    "got a request to repeat"
                );
                let cnt = offsets.len();
                let handle = tokio::spawn(resend_chunks(
                    Arc::clone(path),
                    *size,
                    Arc::clone(socket),
                    id,
                    src,
                    offsets,
                ));
                try_send!((
                    handle,
                    // giving it a semi-unique name for tracking
                    format!("resend_chunks(id={id}, dst={src} cnt={cnt})",)
                ));
            } else {
                tracing::debug!(%id, "ignoring repeat message for unknown session");
            }
        }
        ServerMessage::Done { id } => {
            if sessions.remove(&id).is_some() {
                tracing::info!(%id, "session completed successfully");
            } else {
                tracing::debug!(
                    %id,
                    "ignoring unknown session completion"
                );
            }
        }
        ServerMessage::Announce(src) => {
            tracing::trace!(src, "ignoring announce message");
        }
    }

    Ok(())
}

#[tracing::instrument(level = Level::DEBUG, skip(socket, id), fields(%id) err)]
async fn send_all_chunks(
    path: Arc<PathBuf>,
    size: u64,
    socket: Arc<UdpSocket>,
    id: Identifier,
    dst: SocketAddr,
) -> eyre::Result<()> {
    // we reopen the file so we can seek independently
    let mut file = tokio::fs::File::open(path.as_ref()).await?;

    // holds chunks, which can only grow as large as a datagram payload.
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);
    let mut offset = 0;

    loop {
        tracing::trace!(offset, "sending a chunk");
        let sent = send_chunk(&mut file, &socket, &mut buf, id, offset, dst).await?;
        tracing::trace!("sent {sent} bytes");
        if sent == 0 {
            tracing::warn!("no data sent this iteration, assuming all chunks have been sent");
            break;
        }
        offset += sent; // <= 1452
        match offset.cmp(&size) {
            Ordering::Less => (),
            Ordering::Equal => {
                tracing::debug!("completed sending all chunks");
                break;
            }
            Ordering::Greater => {
                tracing::warn!(
                    offset,
                    size,
                    "sent more data than expected, file might have been modified while being read"
                );
                // while we'd still detect the EOF by reaching sent == 0, the hash will certainly
                // not match, so we can abort this transfer as it's sure to fail
                eyre::bail!("file extended while being read, hash invalidated");
            }
        }
    }

    Ok(())
}

#[tracing::instrument(level = Level::DEBUG, skip(socket, offsets), err)]
async fn resend_chunks(
    path: Arc<PathBuf>,
    size: u64,
    socket: Arc<UdpSocket>,
    id: Identifier,
    dst: SocketAddr,
    offsets: Vec<u64>,
) -> eyre::Result<()> {
    // we reopen the file so we can seek independently
    let mut file = tokio::fs::File::open(path.as_ref()).await?;
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

    for offset in offsets {
        file.seek(io::SeekFrom::Start(offset)).await?;
        tracing::trace!(offset, "re-sending a chunk");
        let sent = send_chunk(&mut file, &socket, &mut buf, id, offset, dst).await?;
        if offset + sent > size {
            tracing::warn!(
                offset,
                size,
                "sent more data than expected, file might have been modified while being read"
            );
            // while we'd still detect the EOF by reaching sent == 0, the hash will certainly
            // not match, so we can abort this transfer as it's sure to fail
            eyre::bail!("file extended while being read, hash invalidated");
        }
    }

    Ok(())
}

async fn send_chunk(
    file: &mut File,
    socket: &UdpSocket,
    buf: &mut Vec<u8>,
    id: Identifier,
    offset: u64,
    dst: SocketAddr,
) -> eyre::Result<u64> {
    buf.clear();
    // :sigh: so read_buf gives no guarantees about whether it will fill up the
    // buffer or not. it often doesn't, in practice. we could use `read_exact`
    // instead, but then there are no guarantees about what happens if the EOF
    // is reached while attempting to fill up the buffer. so to be on the safe
    // side, we fill up the buffer manually until either the buffer is filled or
    // the file runs out of bytes.
    // TODO: it seems we often only fetch 64 bytes or lessat a time, which
    // is not a lot. could actually be faster to do this using synchronous
    // i/o instead.
    let mut read = 0;
    loop {
        let n = file.read_buf(buf).await?;
        read += n;
        if buf.len() == buf.capacity() || n == 0 {
            break;
        }
    }

    let payload = bitcode::encode(&ClientMessage::Data {
        id,
        offset,
        // why not `std::mem::take`? because that resets the capacity to 0!
        content: buf.clone(),
    });
    tracing::trace!("sending a payload of length {}", payload.len());
    // TODO: alas, due to serialization overhead we may actually exceed the
    // datagram payload limit here, leading to fragmentation (or an outright
    // failure in the worst case). to avoid that unfortunately we'll have to
    // drop bitcode and use our own encoding/decoding logic, as we need the
    // overhead to be predictable so we can account for it. this should also
    // make our protocol more portable to other languages.
    let sent = socket.send_to(&payload, dst).await?;
    eyre::ensure!(
        sent == payload.len(),
        "sent {sent} bytes but expected to send {}",
        payload.len()
    );
    // cast safe as buf will be sized `DATAGRAM_SIZE_LIMIT`, which has to fit
    // in a u16 per the IP spec. well, actually there's also a serialization
    // overhead that may make the payload exceed that a bit, but definitely
    // not enough to be of concern. once we implement the TODO above we can be
    // strict about this limit.
    Ok(read as u64)
}

pub async fn send_interactive(ip_version: IpVersion, _path: &Path) {
    let cancel = CancellationToken::new();

    let mut tasks = JoinSet::new();
    tasks.spawn(tui_loop(cancel.clone()));
    tasks.spawn(network_loop(ip_version, cancel.clone()));

    while let Some(r) = tasks.join_next().await {
        if !cancel.is_cancelled() {
            // any task returning means we're shutting down, so let's stop the others
            cancel.cancel();
        }
        match r {
            Ok(Ok(())) => (),
            Ok(Err(err)) => {
                tracing::error!(?err, "task terminated unsuccesfully");
            }
            Err(join_err) => {
                if let Ok(reason) = join_err.try_into_panic() {
                    std::panic::resume_unwind(reason);
                } else {
                    // task cancelled
                }
            }
        }
    }
}

async fn network_loop(ip_version: IpVersion, cancel: CancellationToken) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let socket = create_socket(addr.send, SocketMode::Send)?;

    let discover_msg = bitcode::encode(&ClientMessage::Discover);
    socket.send_to(&discover_msg, addr.recv).await?;

    let mut buf = Vec::with_capacity(65536);
    loop {
        tokio::select! {
            r = socket.recv_buf_from(&mut buf) => {
                let (_, addr) = r?;
                let msg: ServerMessage = bitcode::decode(&buf)?;
                handle_message(addr, msg);
                buf.clear();
            }
            _ = cancel.cancelled() => {
                tracing::debug!("network loop cancelled");
                break;
            }
        }
    }

    Ok(())
}

fn handle_message(src: SocketAddr, msg: ServerMessage) {
    match msg {
        ServerMessage::Announce(name) => {
            tracing::info!(addr = %src, "discovered server: {name}");
        }
        ServerMessage::Ack { nonce, id } => todo!(),
        ServerMessage::Nack { nonce, msg } => todo!(),
        ServerMessage::Error { id, msg } => todo!(),
        ServerMessage::Repeat {
            id,
            offsets: chunks,
        } => todo!(),
        ServerMessage::Done { id } => todo!(),
    }
}

async fn tui_loop(cancel: CancellationToken) -> eyre::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let terminal = Terminal::new(backend)?;
    let mut tui = Tui::new(terminal);
    tui.init()?;

    let mut events = tui.events();
    loop {
        tui.draw()?;
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(event) => match handle_event(event) {
                        Action::Exit => break,
                        Action::None => (),
                    }
                    None => break,
                }
            }
            _ = cancel.cancelled() => {
                tracing::debug!("tui loop cancelled");
                break;
            }
        }
    }

    tui.exit().await?;
    tracing::trace!("tui terminated successfully");

    Ok(())
}

enum Action {
    None,
    Exit,
}

fn handle_event(event: Event) -> Action {
    match event {
        Event::FocusGained => Action::None,
        Event::FocusLost => Action::None,
        Event::Key(event) => {
            if matches!(event.code, KeyCode::Char('q')) {
                Action::Exit
            } else {
                Action::None
            }
        }
        Event::Mouse(_) => Action::None,
        Event::Paste(_) => Action::None,
        Event::Resize(_, _) => Action::None,
    }
}
