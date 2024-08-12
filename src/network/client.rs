use super::{addresses, create_socket, IPVersion, SocketMode};
use crate::{
    file_generator::FileGenerator,
    protocol::{ClientMessage, Hash, ServerMessage},
    tui::Tui,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{backend::CrosstermBackend, Terminal};
use sha2::{Digest, Sha256};
use std::{
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
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

// assuming MTU of 1500 (typical for ethernet), this is
// 1500 - 40 (ipv6 header) - 8 (udp header) = 1452.
// if we estimate this too high, most networks will just
// fragment the packet, which is not the end of the world.
const DATAGRAM_SIZE_LIMIT: usize = 1452;
// set to tokio's default `max_buf_size`
const CHUNK_SIZE: u64 = 2 * 1024 * 1024;

pub async fn send_to_all(ip_version: IPVersion, path: &Path) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let socket = Arc::new(create_socket(addr.send, SocketMode::Send)?);
    let mut files = FileGenerator::new(path);
    let mut tasks = JoinSet::new();
    let mut error_count = 0;

    while let Some(file) = files.next().await {
        let file = match file {
            Ok(file) => file,
            Err(err) => {
                tracing::error!(?err, "filesystem io error");
                error_count += 1;
                continue;
            }
        };
        tasks.spawn(start_sending(file, Arc::clone(&socket), addr.recv));
    }

    while let Some(r) = tasks.join_next().await {
        match r {
            Ok(Ok(())) => (),
            Ok(Err(err)) => {
                tracing::error!(?err, "send task terminated unsuccesfully");
                error_count += 1;
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

    eyre::ensure!(error_count == 0, "{error_count} file(s) could not be sent");

    Ok(())
}

async fn start_sending(path: PathBuf, socket: Arc<UdpSocket>, dst: SocketAddr) -> eyre::Result<()> {
    use rand::Rng;
    let nonce: u32 = rand::thread_rng().gen();

    let mut file = tokio::fs::File::open(&path).await?;
    let size = {
        let meta = file.metadata().await?;
        meta.len()
    };

    let buf_size = size.min(CHUNK_SIZE);
    let mut buf = Vec::with_capacity(buf_size as usize);

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
            .expect("sha256 has 16 bytes exactly")
    };

    // send start message
    {
        let start_msg = {
            let path = path
                .as_os_str()
                .to_str()
                .ok_or_else(|| eyre::eyre!("not a valid utf-8 path: {path:?}"))?;

            let msg = ClientMessage::Start {
                nonce,
                size,
                hash,
                path,
            };

            bitcode::encode(&msg)
        };

        socket.send_to(&start_msg, dst).await?;
    }

    let (task_tx, mut task_rx) = mpsc::unbounded_channel();
    let handler_task = tokio::spawn(handle_server_messages(
        path,
        nonce,
        Arc::clone(&socket),
        buf,
        task_tx.clone(),
    ));
    task_tx.send(handler_task)?;

    while let Some(handle) = task_rx.recv().await {
        match handle.await {
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

    Ok(())
}

async fn handle_server_messages(
    path: PathBuf,
    nonce: u32,
    socket: Arc<UdpSocket>,
    mut buf: Vec<u8>,
    task_sender: mpsc::UnboundedSender<JoinHandle<eyre::Result<()>>>,
) -> eyre::Result<()> {
    macro_rules! try_send {
        ($val:expr) => {
            if task_sender.send($val).is_err() {
                eyre::bail!("handles channel closed, main task terminated prematurely");
            }
        };
    }

    let path = Arc::new(path);
    let mut sessions = HashMap::new();
    loop {
        buf.clear();
        let (_, dst) = socket.recv_buf_from(&mut buf).await?;
        let Ok(msg) = bitcode::decode::<ServerMessage>(&buf) else {
            tracing::error!(?buf, "received invalid message");
            continue;
        };

        match msg {
            ServerMessage::Ack {
                nonce: server_nonce,
                id,
            } => {
                if server_nonce != nonce {
                    tracing::debug!(
                        "ignoring mismatched ack from server. \
                            expected nonce {nonce:#08x} but got {server_nonce:#08x}"
                    );
                    continue;
                }
                sessions.insert(id, dst);
                let handle = tokio::spawn(send_all_chunks(
                    Arc::clone(&path),
                    Arc::clone(&socket),
                    id,
                    dst,
                ));
                try_send!(handle);
            }
            ServerMessage::Nack {
                nonce: server_nonce,
                msg,
            } => {
                if server_nonce != nonce {
                    tracing::debug!(
                        "ignoring mismatched nack from server. \
                            expected nonce {nonce:#08x} but got {server_nonce:#08x}"
                    );
                    continue;
                }
                tracing::error!(msg, "server rejected file transfer");
            }
            ServerMessage::Error { id, msg } => {
                if sessions.remove(&id).is_some() {
                    tracing::error!(msg, "remote server error, session terminated");
                } else {
                    tracing::debug!(msg, "ignoring server error for unknown session {id:#08x}");
                }
            }
            ServerMessage::Repeat { id, chunks } => {
                if let Some(dst) = sessions.get(&id) {
                    let handle = tokio::spawn(resend_chunks(
                        Arc::clone(&path),
                        Arc::clone(&socket),
                        id,
                        *dst,
                        chunks,
                    ));
                    try_send!(handle);
                } else {
                    tracing::debug!("ignoring repeat message for unknown session {id:#08x}");
                }
            }
            ServerMessage::Done { id } => {
                if sessions.remove(&id).is_some() {
                    tracing::info!("completed session {id:#08x} successfully");
                    if sessions.is_empty() {
                        tracing::info!("all sessions completed!");
                        break;
                    }
                } else {
                    tracing::debug!("ignoring unknown session completion {id:#08x}");
                }
            }
            ServerMessage::Announce(src) => {
                tracing::trace!(src, "ignoring announce message");
            }
        }
    }

    Ok(())
}

async fn send_all_chunks(
    path: Arc<PathBuf>,
    socket: Arc<UdpSocket>,
    id: u64,
    dst: SocketAddr,
) -> eyre::Result<()> {
    // we reopen the file so we can seek independently
    let mut file = tokio::fs::File::open(path.as_ref()).await?;

    // holds chunks, which can only grow as large as a datagram payload.
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);
    let mut chunk = 0;

    loop {
        let sent = send_chunk(&mut file, &socket, &mut buf, id, chunk, dst).await?;
        if sent == 0 {
            break;
        }
        chunk += sent as u64; // <= 1452
    }

    Ok(())
}

async fn resend_chunks(
    path: Arc<PathBuf>,
    socket: Arc<UdpSocket>,
    id: u64,
    dst: SocketAddr,
    chunks: Vec<u64>,
) -> eyre::Result<()> {
    // we reopen the file so we can seek independently
    let mut file = tokio::fs::File::open(path.as_ref()).await?;

    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

    for chunk in chunks {
        file.seek(io::SeekFrom::Start(chunk)).await?;
        send_chunk(&mut file, &socket, &mut buf, id, chunk, dst).await?;
    }

    Ok(())
}

async fn send_chunk(
    file: &mut File,
    socket: &UdpSocket,
    buf: &mut Vec<u8>,
    id: u64,
    chunk: u64,
    dst: SocketAddr,
) -> eyre::Result<usize> {
    buf.clear();
    let n = file.read_buf(buf).await?;
    let payload = bitcode::encode(&ClientMessage::Data {
        id,
        chunk,
        content: std::mem::take(buf),
    });
    socket.send_to(&payload, dst).await?;
    Ok(n)
}

pub async fn send_interactive(ip_version: IPVersion, _path: &Path) {
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

async fn network_loop(ip_version: IPVersion, cancel: CancellationToken) -> eyre::Result<()> {
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
        ServerMessage::Repeat { id, chunks } => todo!(),
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
