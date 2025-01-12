use super::{split_udp_socket, UdpSender, UnicastInterface};
use crate::{
    file_generator::FileGenerator,
    protocol::{ClientMessage, ServerMessage, SessionId, DATAGRAM_SIZE_LIMIT},
    tui::Tui,
};
use crossterm::event::{Event, KeyCode};
use rand::{thread_rng, Rng};
use range_set::RangeSet;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    ops::RangeInclusive,
    path::{Path, PathBuf},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

type RangeSetU64 = RangeSet<[RangeInclusive<u64>; 1]>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
struct NeighborId(usize);

pub struct Client {
    tx: mpsc::Sender<ClientCommand>,
}

impl Client {
    pub fn new(addr: SocketAddr) -> Self {
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(client_loop(addr, rx));
        Self { tx }
    }

    pub async fn start_discovery(&self) {
        self.tx
            .send(ClientCommand::StartDiscovery)
            .await
            .expect("client loop running");
    }

    pub async fn stop_discovery(&self) {
        self.tx
            .send(ClientCommand::StopDiscovery)
            .await
            .expect("client loop running");
    }

    pub async fn send(&self, path: PathBuf) -> SessionId {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(ClientCommand::CreateSession { path, reply: tx })
            .await
            .expect("client loop running");
        rx.await.expect("session creation always succeeds")
    }

    pub async fn send_all(&self, path: &Path) -> io::Result<()> {
        let mut file_generator = FileGenerator::new(&path);
        while let Some(res) = file_generator.next().await {
            match res {
                Ok(path) => {
                    self.send(path).await;
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

enum ClientCommand {
    StartDiscovery,
    StopDiscovery,
    CreateSession {
        path: PathBuf,
        reply: oneshot::Sender<SessionId>,
    },
}

struct ClientContext {
    file_cmd_tx: std::sync::mpsc::Sender<FileHandlerCmd>,
    discovery_active: bool,
    sessions: HashMap<SessionId, SessionTask>,
    /// Known neighbors by this client.
    ///
    /// New sessions will be seeded with this set.
    neighbors: HashSet<SocketAddr>,
}

pub struct ServerInfo {
    pub name: String,
    pub addr: SocketAddr,
}

impl ClientContext {
    fn new(file_cmd_tx: std::sync::mpsc::Sender<FileHandlerCmd>) -> Self {
        Self {
            file_cmd_tx,
            sessions: Default::default(),
            discovery_active: Default::default(),
            neighbors: Default::default(),
        }
    }
}

async fn client_loop(
    local_addr: SocketAddr,
    mut cmd_rx: mpsc::Receiver<ClientCommand>,
) -> io::Result<()> {
    let UnicastInterface { socket, mcast_addr } = super::setup_unicast(local_addr)?;
    let (sender, receiver) = split_udp_socket(socket);
    // let mut send_buf = Vec::new();
    let mut recv_buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(packet_sender(sender, rx));

    let (file_cmd_tx, file_cmd_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || file_handler(mcast_addr, tx, file_cmd_rx));

    let mut ctx = ClientContext::new(file_cmd_tx);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    // channel closed, we're shutting down
                    break;
                };
                handle_cmd(&mut ctx, cmd).await?;
            }
            r = receiver.recv_from(&mut recv_buf) => {
                let (len, src) = r?;
                let msg = match ServerMessage::decode(&recv_buf[..len]) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::error!(?err, ?recv_buf, "received corrupt message");
                        continue;
                    }
                };
                handle_server_msg(&mut ctx, msg, src).await;
            }
        }
    }

    Ok(())
}

async fn handle_server_msg(ctx: &mut ClientContext, msg: ServerMessage<'_>, src: SocketAddr) {
    match msg {
        ServerMessage::Announce(name) => {
            if !ctx.discovery_active {
                return;
            }
            tracing::info!("discovered neighbor: {name}");
            ctx.neighbors.insert(src);
            // TODO: parallelize
            let mut ended = Vec::new();
            for (session_id, session) in &ctx.sessions {
                if session
                    .tx
                    .send(SessionCommand::AddNeighbor(src))
                    .await
                    .is_err()
                {
                    ended.push(*session_id);
                }
            }
            for session_id in ended {
                ctx.sessions.remove(&session_id);
            }
        }
        ServerMessage::Ack { session_id } => {
            if ctx.sessions.get(&session_id).is_none() {
                tracing::debug!(%src, %session_id, "got ack for unknown session");
            }
        }
        ServerMessage::Nack { session_id, msg } => {
            let Some(session) = ctx.sessions.get(&session_id) else {
                tracing::debug!(%src, %session_id, "got nack for unknown session");
                return;
            };
            tracing::warn!(%src, %session_id, "neighbor rejected session: {msg}");
            if session
                .tx
                .send(SessionCommand::RemoveNeighbor(src))
                .await
                .is_err()
            {
                // session terminated
                ctx.sessions.remove(&session_id);
            }
        }
        ServerMessage::Error { session_id, msg } => {
            let Some(session) = ctx.sessions.get(&session_id) else {
                tracing::debug!(%src, %session_id, "got error for unknown session");
                return;
            };
            tracing::warn!(%src, %session_id, "neighbor produced error: {msg}");
            if session
                .tx
                .send(SessionCommand::RemoveNeighbor(src))
                .await
                .is_err()
            {
                // session terminated
                ctx.sessions.remove(&session_id);
            }
        }
        ServerMessage::Repeat {
            session_id,
            offsets,
        } => {
            let Some(session) = ctx.sessions.get(&session_id) else {
                tracing::debug!(%src, %session_id, "got repeat request with unknown session");
                return;
            };
            if session
                .tx
                .send(SessionCommand::Retransmit {
                    session_id,
                    addr: src,
                    chunks: offsets.into_owned(),
                })
                .await
                .is_err()
            {
                // session terminated
                ctx.sessions.remove(&session_id);
            }
        }
        ServerMessage::Done { session_id } => {
            let Some(session) = ctx.sessions.get(&session_id) else {
                tracing::debug!(%src, %session_id, "unknown transfer completed");
                return;
            };
            if session
                .tx
                .send(SessionCommand::RemoveNeighbor(src))
                .await
                .is_err()
            {
                // session terminated
                ctx.sessions.remove(&session_id);
            }
        }
    }
}

async fn handle_cmd(ctx: &mut ClientContext, cmd: ClientCommand) -> io::Result<()> {
    match cmd {
        ClientCommand::StartDiscovery => {
            ctx.discovery_active = true;
        }
        ClientCommand::StopDiscovery => {
            ctx.discovery_active = false;
        }
        ClientCommand::CreateSession { path, reply } => {
            let (tx, rx) = mpsc::channel(8);
            let session_id = thread_rng().gen();
            let neighs = ctx.neighbors.clone();
            let handle = tokio::spawn(session_loop(
                session_id,
                path,
                ctx.file_cmd_tx.clone(),
                neighs,
                rx,
            ));
            ctx.sessions.insert(session_id, SessionTask { tx, handle });
            let _ = reply.send(session_id).is_ok();
        }
    }
    Ok(())
}

struct SessionTask {
    tx: mpsc::Sender<SessionCommand>,
    handle: JoinHandle<()>,
}

enum SessionCommand {
    AddNeighbor(SocketAddr),
    RemoveNeighbor(SocketAddr),
    Retransmit {
        session_id: SessionId,
        addr: SocketAddr,
        chunks: Vec<u64>,
    },
}

struct SessionContext {
    path: PathBuf,
    file_cmd_tx: std::sync::mpsc::Sender<FileHandlerCmd>,
    neighbors: HashSet<SocketAddr>,
}

enum SessionInstruction {
    Stop,
    Continue,
}

async fn session_loop(
    session_id: SessionId,
    path: PathBuf,
    file_cmd_tx: std::sync::mpsc::Sender<FileHandlerCmd>,
    neighbors: HashSet<SocketAddr>,
    mut rx_cmd: mpsc::Receiver<SessionCommand>,
) {
    let _ = file_cmd_tx.send(FileHandlerCmd::Broadcast {
        session_id,
        path: path.clone(),
    });

    let mut ctx = SessionContext {
        path,
        neighbors,
        file_cmd_tx,
    };
    loop {
        tokio::select! {
            cmd = rx_cmd.recv() => {
                let Some(cmd) = cmd else {
                    // must be shutting down
                    break;
                };
                match handle_session_cmd(&mut ctx, cmd).await {
                    SessionInstruction::Stop => break,
                    SessionInstruction::Continue => continue,
                }
            }
        }
    }
}

async fn handle_session_cmd(ctx: &mut SessionContext, cmd: SessionCommand) -> SessionInstruction {
    match cmd {
        SessionCommand::AddNeighbor(addr) => {
            // late joiners may have missed some or all of the broadcast
            // packets... this is fine - we'll rely on retransmit requests to
            // make sure they get the data they need
            // TODO: efficient chunks encoding for contiguous spans
            ctx.neighbors.insert(addr);
        }
        SessionCommand::RemoveNeighbor(addr) => {
            ctx.neighbors.remove(&addr);
            // all neighbors have either received all data, or produced an
            // irrecoverable error
            if ctx.neighbors.is_empty() {
                return SessionInstruction::Stop;
            }
        }
        SessionCommand::Retransmit {
            session_id,
            addr,
            chunks,
        } => {
            let _ = ctx.file_cmd_tx.send(FileHandlerCmd::TransmitChunks {
                path: ctx.path.clone(),
                chunks,
                session_id,
                addr,
            });
        }
    }
    SessionInstruction::Continue
}

enum FileHandlerCmd {
    Broadcast {
        session_id: SessionId,
        path: PathBuf,
    },
    TransmitChunks {
        session_id: SessionId,
        path: PathBuf,
        chunks: Vec<u64>,
        addr: SocketAddr,
    },
}

struct UdpPayload {
    data: Vec<u8>,
    dst: SocketAddr,
}

// this runs in a separate thread because file IO is intrinsically blocking
// (`tokio::fs` simply spawns a blocking task for every read, which is awful
// for accessing tiny chunks of data like we do here)
fn file_handler(
    mcast_addr: SocketAddr,
    sender: mpsc::UnboundedSender<UdpPayload>,
    cmd_rx: std::sync::mpsc::Receiver<FileHandlerCmd>,
) {
    let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);
    loop {
        let Ok(cmd) = cmd_rx.recv() else {
            // must be shutting down
            return;
        };
        match cmd {
            FileHandlerCmd::Broadcast { path, session_id } => {
                let mut count = 0;
                let mut offset = 0;
                let mut file = match std::fs::File::open(&path) {
                    Ok(file) => file,
                    Err(err) => {
                        tracing::error!(
                            ?err,
                            path = %path.display(),
                            "failed to open file, broadcast aborted"
                        );
                        continue;
                    }
                };
                let meta = match file.metadata() {
                    Ok(meta) => meta,
                    Err(err) => {
                        tracing::error!(
                            ?err,
                            path = %path.display(),
                            "failed to fetch file stats, broadcast aborted"
                        );
                        continue;
                    }
                };
                while offset < meta.len() {
                    buf.clear();
                    let written = match ClientMessage::encode_data_msg_sync(
                        session_id, offset, &mut file, &mut buf,
                    ) {
                        Ok(written) => written,
                        Err(err) => {
                            tracing::error!(?err, sent = count, offset, path = %path.display(), "failed to read file, broadcast aborted");
                            continue;
                        }
                    };
                    sender.send(UdpPayload {
                        // TODO: is there a way to avoid allocating a brand new
                        // buffer here for every packet sent? presumably could
                        // send a oneshot so the original buffer is returned,
                        // but then we block progress here...
                        data: buf.clone(),
                        dst: mcast_addr,
                    });
                    offset += written as u64;
                    count += 1;
                }
            }
            FileHandlerCmd::TransmitChunks {
                session_id,
                path,
                chunks,
                addr,
            } => todo!(),
        }
    }
}

async fn packet_sender(sender: UdpSender, mut tx: mpsc::UnboundedReceiver<UdpPayload>) {
    while let Some(UdpPayload { data, dst }) = tx.recv().await {
        sender.send_to(&data[..], dst).await;
    }
}

// pub async fn broadcast_from_path(
//     unicast_addr: SocketAddr,
//     path: &Path,
//     grace_period: Duration,
// ) -> eyre::Result<()> {
//     let mut tasks = Vec::new();
//     let mut task_rx = {
//         let UnicastInterface { socket, mcast_addr } = super::setup_unicast(unicast_addr)?;
//         let (task_tx, task_rx) = mpsc::unbounded_channel();
//         let handle = tokio::spawn(broadcast_all(
//             socket,
//             mcast_addr,
//             FileGenerator::new(path),
//             task_tx,
//             grace_period,
//         ));
//         tasks.push((handle, "broadcast_all".into()));
//         task_rx
//     };

//     while let Some((handle, ref name)) = tasks.last_mut() {
//         tokio::select! {
//             // NOTE: this arm will be disabled when the channel closes
//             Some(task) = task_rx.recv() => {
//                 tasks.push(task);
//             }
//             r = handle => {
//                 match r {
//                     Ok(Ok(())) => {
//                         tracing::debug!("task {name} completed successfully");
//                     }
//                     Ok(Err(err)) => {
//                         tracing::error!(?err, "task {name} terminated unsuccesfully");
//                     }
//                     Err(join_err) => {
//                         if join_err.is_cancelled() {
//                             continue;
//                         }
//                         std::panic::resume_unwind(join_err.into_panic());
//                     }
//                 }
//                 tasks.pop();
//             }
//         }
//     }

//     Ok(())
// }

// #[derive(Debug, Default)]
// enum GraceStatus {
//     #[default]
//     Waiting,
//     Started {
//         end_at: Instant,
//     },
//     Ended,
// }

// impl GraceStatus {
//     fn end(&self) -> Option<Instant> {
//         match self {
//             GraceStatus::Waiting => None,
//             GraceStatus::Started { end_at } => Some(*end_at),
//             GraceStatus::Ended => None,
//         }
//     }
// }

// // NOTE: this task must be the exclusive reader of the socket
// async fn broadcast_all(
//     socket: UdpSocket,
//     mcast_addr: SocketAddr,
//     paths: FileGenerator,
//     task_sender: mpsc::UnboundedSender<(JoinHandle<eyre::Result<()>>, String)>,
//     grace_period: Duration,
// ) -> eyre::Result<()> {
//     let socket = Arc::new(socket);
//     let mut grace = GraceStatus::default();
//     let mut error_count = 0;
//     let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);
//     let mut pending_sessions = HashMap::new();
//     let mut sessions = HashMap::new();
//     tokio::pin!(paths);
//     loop {
//         buf.clear();
//         let grace_end = grace.end();
//         tokio::select! {
//             // if we go 5 seconds without any of the other futures completing,
//             // either the network is very bad or we got a bug
//             _ = tokio::time::sleep(Duration::from_secs(5)) => {
//                 tracing::warn!(
//                     pending_sessions = ?pending_sessions.keys().collect::<Vec<_>>(),
//                     active_sessions = ?sessions.keys().collect::<Vec<_>>(),
//                     "no progress in the last 5 seconds, we might be stuck"
//                 );
//             }
//             // track when the grace period expires
//             _ = async { tokio::time::sleep_until(grace_end.unwrap()).await },
//                 if grace_end.is_some() => {
//                 tracing::trace!("grace period ended");
//                 grace = GraceStatus::Ended;
//                 if sessions.is_empty() {
//                     tracing::info!("completed all sessions");
//                     break;
//                 } else {
//                     tracing::trace!(
//                         sessions = ?sessions.keys().collect::<Vec<_>>(),
//                         "there are still active sessions"
//                     );
//                 }
//             }
//             // NOTE: arm will be *disabled* when paths is exhausted
//             Some(r) = paths.next() => {
//                 let path = match r {
//                     Ok(path) => path,
//                     Err(err) => {
//                         tracing::error!(?err, "filesystem io error");
//                         error_count += 1;
//                         continue;
//                     }
//                 };
//                 let (session, size) = start_session(&path, &socket, mcast_addr).await?;
//                 // every new session we start renews the grace period
//                 let end_at = Instant::now() + grace_period;
//                 tracing::trace!(?end_at, "grace period renewed");
//                 grace = GraceStatus::Started { end_at  };
//                 pending_sessions.insert(session, (path, size));
//             }
//             r = socket.recv_buf_from(&mut buf) => {
//                 let (_, src) = r?;
//                 let msg = match  ServerMessage::decode(&buf) {
//                     Ok(msg) => msg,
//                     Err(err) => {
//                         tracing::error!(?err, ?buf, "received invalid message");
//                         continue;
//                     }
//                 };
//                 handle_msg(
//                     src,
//                     msg,
//                     &mut pending_sessions,
//                     &mut sessions,
//                     &socket,
//                     mcast_addr,
//                     &task_sender
//                 ).await?;
//                 // make sure we only quit after the grace period is over
//                 if sessions.is_empty() && matches!(grace, GraceStatus::Ended) {
//                     tracing::info!("completed all sessions!");
//                     break;
//                 } else if sessions.is_empty() {
//                     tracing::debug!("sessions empty but grace period hasn't ended yet");
//                 }
//             }
//         }
//     }

//     eyre::ensure!(error_count == 0, "{error_count} error(s) detected");

//     Ok(())
// }

// async fn start_session(
//     path: &Path,
//     socket: &UdpSocket,
//     bcast_addr: SocketAddr,
// ) -> eyre::Result<(SessionId, u64)> {
//     tracing::info!(path = %path.display(), %bcast_addr, "starting a broadcast file transfer");

//     let session = rand::thread_rng().gen();
//     let mut buf;

//     let file = tokio::fs::File::open(&path).await?;
//     let size = {
//         let meta = file.metadata().await?;
//         meta.len()
//     };

//     buf = Vec::with_capacity(HASHING_CHUNK_SIZE.min(size as usize));

//     let hash = super::hash(file, size as usize, &mut buf).await?;

//     // send start message
//     let path = path
//         .as_os_str()
//         .to_str()
//         .ok_or_else(|| eyre::eyre!("not a valid utf-8 path: {path:?}"))?;

//     tracing::debug!(%session, %path, size, %hash, "sending start message");

//     buf.clear();
//     ClientMessage::Start {
//         session_id: session,
//         size,
//         hash,
//         path,
//     }
//     .encode(&mut buf)
//     .expect("vec grows as needed");

//     socket.send_to(&buf, bcast_addr).await?;

//     Ok((session, size))
// }

// async fn handle_msg(
//     src: SocketAddr,
//     msg: ServerMessage<'_>,
//     pending: &mut HashMap<SessionId, (PathBuf, u64)>,
//     sessions: &mut HashMap<SessionId, (Arc<PathBuf>, u64)>,
//     socket: &Arc<UdpSocket>,
//     bcast_addr: SocketAddr,
//     task_sender: &mpsc::UnboundedSender<(JoinHandle<eyre::Result<()>>, String)>,
// ) -> eyre::Result<()> {
//     macro_rules! try_send {
//         ($val:expr) => {
//             if task_sender.send($val).is_err() {
//                 eyre::bail!("handles channel closed, main task terminated prematurely");
//             }
//         };
//     }

//     match msg {
//         ServerMessage::Ack { session_id } => {
//             if let Some((path, size)) = pending.remove(&session_id) {
//                 tracing::info!(
//                     %session_id,
//                     %transfer,
//                     "file transfer session started"
//                 );
//                 let path = Arc::new(path);
//                 let handle = tokio::spawn(send_all_chunks(
//                     Arc::clone(&path),
//                     size,
//                     Arc::clone(socket),
//                     transfer,
//                     bcast_addr,
//                 ));
//                 sessions.insert(transfer, (path, size));
//                 try_send!((handle, format!("send_all_chunks(id={transfer})")));
//             } else {
//                 tracing::debug!(%session_id, "ignoring ack with unknown session");
//             }
//         }
//         ServerMessage::Nack {
//             session_id: session,
//             msg,
//         } => {
//             if pending.remove(&session).is_some() {
//                 tracing::error!(msg, "server rejected file transfer");
//             } else {
//                 tracing::debug!("ignoring nack with unknown session");
//             }
//         }
//         ServerMessage::Error {
//             transfer_id: transfer,
//             msg,
//         } => {
//             if sessions.remove(&transfer).is_some() {
//                 tracing::error!(%transfer, msg, "remote server error, session terminated");
//             } else {
//                 tracing::debug!(%transfer, msg, "ignoring server error for unknown session");
//             }
//         }
//         ServerMessage::Repeat {
//             transfer_id: transfer,
//             offsets,
//         } => {
//             if let Some((path, size)) = sessions.get(&transfer) {
//                 tracing::debug!(
//                     %transfer,
//                     ?offsets,
//                     "got a request to repeat"
//                 );
//                 let cnt = offsets.len();
//                 let handle = tokio::spawn(resend_chunks(
//                     Arc::clone(path),
//                     *size,
//                     Arc::clone(socket),
//                     transfer,
//                     src,
//                     offsets.to_vec(),
//                 ));
//                 try_send!((
//                     handle,
//                     // giving it a semi-unique name for tracking
//                     format!("resend_chunks(id={transfer}, dst={src} cnt={cnt})",)
//                 ));
//             } else {
//                 tracing::debug!(%transfer, "ignoring repeat message for unknown session");
//             }
//         }
//         ServerMessage::Done {
//             transfer_id: transfer,
//         } => {
//             if sessions.remove(&transfer).is_some() {
//                 tracing::info!(%transfer, "session completed successfully");
//             } else {
//                 tracing::debug!(
//                     %transfer,
//                     "ignoring unknown session completion"
//                 );
//             }
//         }
//         ServerMessage::Announce(src) => {
//             tracing::trace!(src, "ignoring announce message");
//         }
//     }

//     Ok(())
// }

// #[tracing::instrument(level = Level::DEBUG, skip(socket, id), fields(%id) err)]
// async fn send_all_chunks(
//     path: Arc<PathBuf>,
//     file_size: u64,
//     socket: Arc<UdpSocket>,
//     id: TransferId,
//     dst: SocketAddr,
// ) -> eyre::Result<()> {
//     // we reopen the file so we can seek independently
//     let mut file = tokio::fs::File::open(path.as_ref()).await?;

//     // total size = tag (1 byte) + id (8 bytes) + offset (8 bytes) + content
//     let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

//     let mut count = 0;
//     let mut offset = 0;
//     loop {
//         tracing::trace!(offset, "sending chunk");
//         let sent = send_chunk(&mut file, &socket, &mut buf, id, offset, dst).await?;
//         tracing::trace!("sent {sent} bytes");
//         debug_assert!(
//             sent == CHUNK_SIZE || (sent < CHUNK_SIZE && offset + CHUNK_SIZE > file_size),
//             "unexpected chunk size {sent} at offset {offset} \
//             (count={count}, total_size={file_size})"
//         );
//         offset += sent; // <= 1452
//         count += 1;
//         if sent == 0 {
//             tracing::warn!(
//                 count,
//                 offset,
//                 "no data sent this iteration, assuming all chunks have been sent"
//             );
//             break;
//         }
//         match offset.cmp(&file_size) {
//             Ordering::Less => (),
//             Ordering::Equal => {
//                 tracing::debug!(count, total_sent = offset, "completed sending all chunks");
//                 break;
//             }
//             Ordering::Greater => {
//                 tracing::warn!(
//                     count,
//                     total_sent = offset,
//                     file_size,
//                     "sent more data than expected, file might have been modified while being read"
//                 );
//                 // while we'd still detect the EOF by reaching sent == 0, the hash will certainly
//                 // not match, so we can abort this transfer as it's sure to fail
//                 eyre::bail!("file extended while being read, hash invalidated");
//             }
//         }
//     }

//     Ok(())
// }

// #[tracing::instrument(level = Level::DEBUG, skip(socket, offsets), err)]
// async fn resend_chunks(
//     path: Arc<PathBuf>,
//     size: u64,
//     socket: Arc<UdpSocket>,
//     id: TransferId,
//     dst: SocketAddr,
//     offsets: Vec<u64>,
// ) -> eyre::Result<()> {
//     // we reopen the file so we can seek independently
//     let mut file = tokio::fs::File::open(path.as_ref()).await?;

//     // total size = tag (1 byte) + id (8 bytes) + offset (8 bytes) + content (read bytes)
//     let mut buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

//     for offset in offsets {
//         file.seek(io::SeekFrom::Start(offset)).await?;
//         tracing::trace!(offset, "re-sending a chunk");
//         let sent = send_chunk(&mut file, &socket, &mut buf, id, offset, dst).await?;
//         if offset + sent > size {
//             tracing::warn!(
//                 offset,
//                 size,
//                 "sent more data than expected, file might have been modified while being read"
//             );
//             // while we'd still detect the EOF by reaching sent == 0, the hash will certainly
//             // not match, so we can abort this transfer as it's sure to fail
//             eyre::bail!("file extended while being read, hash invalidated");
//         }
//     }

//     Ok(())
// }

// async fn send_chunk(
//     file: &mut File,
//     socket: &UdpSocket,
//     buf: &mut Vec<u8>,
//     id: TransferId,
//     offset: u64,
//     dst: SocketAddr,
// ) -> eyre::Result<u64> {
//     debug_assert!(buf.capacity() == DATAGRAM_SIZE_LIMIT);

//     buf.clear();
//     let read = ClientMessage::encode_data_msg(id, offset, file, buf).await?;

//     tracing::trace!(
//         payload_len = read,
//         "sending a message of length {}",
//         buf.len()
//     );

//     let sent = socket.send_to(buf, dst).await?;
//     eyre::ensure!(
//         sent == buf.len(),
//         "sent {sent} bytes but expected to send {}",
//         buf.len()
//     );

//     // cast safe as buf will be sized `DATAGRAM_SIZE_LIMIT`, which has to fit
//     // in a u16 per the IP spec.
//     Ok(read as u64)
// }

// pub async fn send_interactive(unicast_addr: SocketAddr, _path: &Path) {
//     let cancel = CancellationToken::new();

//     let mut tasks = JoinSet::new();
//     tasks.spawn(tui_loop(cancel.clone()));
//     tasks.spawn(network_loop(unicast_addr, cancel.clone()));

//     while let Some(r) = tasks.join_next().await {
//         if !cancel.is_cancelled() {
//             // any task returning means we're shutting down, so let's stop the others
//             cancel.cancel();
//         }
//         match r {
//             Ok(Ok(())) => (),
//             Ok(Err(err)) => {
//                 tracing::error!(?err, "task terminated unsuccesfully");
//             }
//             Err(join_err) => {
//                 if let Ok(reason) = join_err.try_into_panic() {
//                     std::panic::resume_unwind(reason);
//                 } else {
//                     // task cancelled
//                 }
//             }
//         }
//     }
// }

// async fn network_loop(unicast_addr: SocketAddr, cancel: CancellationToken) -> eyre::Result<()> {
//     let UnicastInterface { socket, mcast_addr } = super::setup_unicast(unicast_addr)?;
//     let mut buf = Vec::with_capacity(65536);

//     ClientMessage::Discover
//         .encode(&mut buf)
//         .expect("vec grows as needed");
//     socket.send_to(&buf, mcast_addr).await?;

//     loop {
//         tokio::select! {
//             r = socket.recv_buf_from(&mut buf) => {
//                 let (_, addr) = r?;
//                 let msg = ServerMessage::decode(&buf)?;
//                 handle_message(addr, msg);
//                 buf.clear();
//             }
//             _ = cancel.cancelled() => {
//                 tracing::debug!("network loop cancelled");
//                 break;
//             }
//         }
//     }

//     Ok(())
// }

// fn handle_message(src: SocketAddr, msg: ServerMessage) {
//     todo!()
// }

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
