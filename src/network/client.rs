use super::{addresses, create_socket, IpVersion, SocketMode};
use crate::{
    file_generator::FileGenerator,
    protocol::{
        ClientMessage, Hash, Identifier, Nonce, ServerMessage, CHUNK_SIZE, DATAGRAM_SIZE_LIMIT,
    },
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
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    net::UdpSocket,
    sync::mpsc,
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tracing::Level;

pub async fn broadcast_from_path(
    ip_version: IpVersion,
    path: &Path,
    grace_period: Duration,
) -> eyre::Result<()> {
    let mut tasks = Vec::new();
    let mut task_rx = {
        let addr = addresses(ip_version);
        let socket = create_socket(addr.send, SocketMode::Send)?;
        let (task_tx, task_rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(broadcast_all(
            socket,
            addr.recv,
            FileGenerator::new(path),
            task_tx,
            grace_period,
        ));
        tasks.push((handle, "broadcast_all".into()));
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

#[derive(Debug, Default)]
enum GraceStatus {
    #[default]
    Waiting,
    Started {
        end_at: Instant,
    },
    Ended,
}

impl GraceStatus {
    fn end(&self) -> Option<Instant> {
        match self {
            GraceStatus::Waiting => None,
            GraceStatus::Started { end_at } => Some(*end_at),
            GraceStatus::Ended => None,
        }
    }
}

// NOTE: this task must be the exclusive reader of the socket
async fn broadcast_all(
    socket: UdpSocket,
    bcast_addr: SocketAddr,
    paths: FileGenerator,
    task_sender: mpsc::UnboundedSender<(JoinHandle<eyre::Result<()>>, String)>,
    grace_period: Duration,
) -> eyre::Result<()> {
    let socket = Arc::new(socket);
    let mut grace = GraceStatus::default();
    let mut error_count = 0;
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);
    let mut pending_sessions = HashMap::new();
    let mut sessions = HashMap::new();
    tokio::pin!(paths);
    loop {
        buf.clear();
        let grace_end = grace.end();
        tokio::select! {
            // if we go 5 seconds without any of the other futures completing,
            // either the network is very bad or we got a bug
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                tracing::warn!(
                    pending_sessions = ?pending_sessions.keys().collect::<Vec<_>>(),
                    active_sessions = ?sessions.keys().collect::<Vec<_>>(),
                    "no progress in the last 5 seconds, we might be stuck"
                );
            }
            // track when the grace period expires
            _ = async { tokio::time::sleep_until(grace_end.unwrap()).await },
                if grace_end.is_some() => {
                tracing::trace!("grace period ended");
                grace = GraceStatus::Ended;
                if sessions.is_empty() {
                    tracing::info!("completed all sessions");
                    break;
                } else {
                    tracing::trace!(
                        sessions = ?sessions.keys().collect::<Vec<_>>(),
                        "there are still active sessions"
                    );
                }
            }
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
                // every new session we start renews the grace period
                let end_at = Instant::now() + grace_period;
                tracing::trace!(?end_at, "grace period renewed");
                grace = GraceStatus::Started { end_at  };
                pending_sessions.insert(nonce, (path, size));
            }
            r = socket.recv_buf_from(&mut buf) => {
                let (_, src) = r?;
                let msg = match  ServerMessage::decode(&buf) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::error!(?err, ?buf, "received invalid message");
                        continue;
                    }
                };
                handle_msg(
                    src,
                    msg,
                    &mut pending_sessions,
                    &mut sessions,
                    &socket,
                    bcast_addr,
                    &task_sender
                ).await?;
                // make sure we only quit after the grace period is over
                if sessions.is_empty() && matches!(grace, GraceStatus::Ended) {
                    tracing::info!("completed all sessions!");
                    break;
                } else if sessions.is_empty() {
                    tracing::debug!("sessions empty but grace period hasn't ended yet");
                }
            }
        }
    }

    eyre::ensure!(error_count == 0, "{error_count} error(s) detected");

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
    let path = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| eyre::eyre!("not a valid utf-8 path: {path:?}"))?;

    tracing::debug!(%nonce, %path, size, %hash, "sending start message");

    buf.clear();
    ClientMessage::Start {
        nonce,
        size,
        hash,
        path,
    }
    .encode(&mut buf)
    .expect("vec grows as needed");

    socket.send_to(&buf, bcast_addr).await?;

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
                    offsets.to_vec(),
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
    file_size: u64,
    socket: Arc<UdpSocket>,
    id: Identifier,
    dst: SocketAddr,
) -> eyre::Result<()> {
    // we reopen the file so we can seek independently
    let mut file = tokio::fs::File::open(path.as_ref()).await?;

    // total size = tag (1 byte) + id (8 bytes) + offset (8 bytes) + content
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

    let mut count = 0;
    let mut offset = 0;
    loop {
        tracing::trace!(offset, "sending chunk");
        let sent = send_chunk(&mut file, &socket, &mut buf, id, offset, dst).await?;
        tracing::trace!("sent {sent} bytes");
        debug_assert!(
            sent == CHUNK_SIZE || offset + CHUNK_SIZE > file_size,
            "unexpected chunk size {sent} at offset {offset} \
            (count={count}, total_size={file_size})"
        );
        offset += sent; // <= 1452
        count += 1;
        if sent == 0 {
            tracing::warn!(
                count,
                offset,
                "no data sent this iteration, assuming all chunks have been sent"
            );
            break;
        }
        match offset.cmp(&file_size) {
            Ordering::Less => (),
            Ordering::Equal => {
                tracing::debug!(count, total_sent = offset, "completed sending all chunks");
                break;
            }
            Ordering::Greater => {
                tracing::warn!(
                    count,
                    total_sent = offset,
                    file_size,
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

    // total size = tag (1 byte) + id (8 bytes) + offset (8 bytes) + content (read bytes)
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
    debug_assert!(buf.capacity() == DATAGRAM_SIZE_LIMIT);

    buf.clear();
    let read = ClientMessage::encode_data_msg(id, offset, file, buf).await?;

    tracing::trace!(
        payload_len = read,
        "sending a message of length {}",
        buf.len()
    );

    let sent = socket.send_to(buf, dst).await?;
    eyre::ensure!(
        sent == buf.len(),
        "sent {sent} bytes but expected to send {}",
        buf.len()
    );

    // cast safe as buf will be sized `DATAGRAM_SIZE_LIMIT`, which has to fit
    // in a u16 per the IP spec.
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
    let mut buf = Vec::with_capacity(65536);

    ClientMessage::Discover
        .encode(&mut buf)
        .expect("vec grows as needed");
    socket.send_to(&buf, addr.recv).await?;

    loop {
        tokio::select! {
            r = socket.recv_buf_from(&mut buf) => {
                let (_, addr) = r?;
                let msg = ServerMessage::decode(&buf)?;
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
    todo!()
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
