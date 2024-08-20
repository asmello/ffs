use super::{addresses, create_socket, IpVersion, SocketMode};
use crate::protocol::{
    ClientMessage, Hash, Identifier, ServerMessage, CHUNK_SIZE, DATAGRAM_SIZE_LIMIT,
};
use itertools::Itertools;
use rand::Rng;
use range_set::RangeSet;
use std::{
    cmp::Ordering, collections::HashMap, io, net::SocketAddr, ops::RangeInclusive, path::Path,
    time::Duration,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncSeekExt, AsyncWriteExt},
    net::UdpSocket,
    time::Instant,
};
use tracing::Level;

const REPEAT_REQUEST_DELAY: Duration = Duration::from_millis(100);

// TODO: use a better tailored (and better maintained) implementation
type RangeSetU64 = RangeSet<[RangeInclusive<u64>; 1]>;

struct FileEntry {
    src: SocketAddr,
    file: File,
    hash: Hash<'static>,
    curr_size: usize,
    expected_size: usize,
    received: RangeSetU64,
}

struct ServerContext<'name> {
    name: &'name str,
    unicast_socket: UdpSocket,
    sessions: HashMap<Identifier, FileEntry>,
    open_opts: OpenOptions,
    request_repetitions_at: Option<Instant>,
}

pub async fn serve(name: &str, ip_version: IpVersion, overwrite: bool) -> eyre::Result<()> {
    let addr = addresses(ip_version);
    let multicast_socket = create_socket(addr.recv, SocketMode::Receive)?;
    let local_addr = multicast_socket.local_addr().unwrap();
    let mut open_opts = tokio::fs::OpenOptions::new();
    open_opts.write(true).read(false);
    if overwrite {
        open_opts.create(true).truncate(true);
    } else {
        open_opts.create_new(true);
    }
    let mut ctx = ServerContext {
        name,
        unicast_socket: create_socket(addr.send, SocketMode::Send)?,
        sessions: HashMap::new(),
        open_opts,
        request_repetitions_at: None,
    };

    // we could avoid a second read buffer if we used [`UdpSocket::readable`], but that'd
    // require 2 syscalls per message so it's probably slower.
    // TODO: benchmark to validate this assumption
    let mut mc_read_buf = Vec::with_capacity(65535);
    let mut uc_read_buf = Vec::with_capacity(65535);
    let mut write_buf = Vec::with_capacity(DATAGRAM_SIZE_LIMIT);

    tracing::info!("listening on {local_addr:?}");

    macro_rules! decode_and_handle_msg {
        ($ret:ident, $buf:ident) => {
            let (_, addr) = $ret?;
            let Ok(msg) = ClientMessage::decode(&$buf) else {
                tracing::error!(?$buf, "invalid message received");
                continue;
            };
            handle_message(&mut ctx, addr, msg, &mut write_buf).await;
        };
    }

    loop {
        mc_read_buf.clear();
        uc_read_buf.clear();
        tokio::select! {
            // catch-all for broadcast messages
            r = multicast_socket.recv_buf_from(&mut mc_read_buf) => {
                decode_and_handle_msg!(r, mc_read_buf);
            }
            // used to track replies to repetition requests and unicast transfers
            r = ctx.unicast_socket.recv_buf_from(&mut uc_read_buf) => {
                decode_and_handle_msg!(r, uc_read_buf);
            }
            // triggered once a gap is detected in any file
            _ = async { tokio::time::sleep_until(ctx.request_repetitions_at.unwrap()).await },
                if ctx.request_repetitions_at.is_some() => {
                request_repetitions(&mut ctx, &mut write_buf).await;
                ctx.request_repetitions_at = None;
            }
            // if we have active sessions but go 5 seconds without receiving any
            // messages, odds are we got a problem
            _ = tokio::time::sleep(Duration::from_secs(5)), if !ctx.sessions.is_empty() => {
                tracing::warn!(
                    ids = ?ctx.sessions.keys().collect::<Vec<_>>(),
                    "no progress in the last 5 seconds, we might be stuck"
                );
            }
        }
    }
}

async fn request_repetitions(ctx: &mut ServerContext<'_>, buf: &mut Vec<u8>) {
    for (&id, entry) in &ctx.sessions {
        if !has_gaps(&entry.received) {
            continue;
        }

        tracing::trace!(%id, bytes = ?entry.received, "bytes received so far");

        let ranges = entry.received.as_ref();
        // TODO: is it worth making this a permanent buffer?
        let mut offsets = Vec::with_capacity(ranges.len());
        for (a, b) in ranges.iter().tuple_windows() {
            let mut offset = *a.end() + 1;
            loop {
                offsets.push(offset);
                if offset + CHUNK_SIZE < *b.start() {
                    offset += CHUNK_SIZE;
                } else {
                    debug_assert_eq!(offset + CHUNK_SIZE, *b.start());
                    break;
                }
            }
        }
        // TODO: handle missing first or last chunks
        tracing::debug!(%id, ?offsets, "requesting repetitions");
        try_send(
            ServerMessage::Repeat {
                id,
                offsets: offsets.into(),
            },
            &ctx.unicast_socket,
            entry.src,
            buf,
        )
        .await;
    }
}

fn has_gaps(set: &RangeSetU64) -> bool {
    set.as_ref().len() > 1
}

async fn try_send(
    msg: ServerMessage<'_>,
    socket: &UdpSocket,
    dst: SocketAddr,
    buf: &mut Vec<u8>,
) -> bool {
    buf.clear();
    msg.encode(buf).expect("vec grows as needed");
    if let Err(err) = socket.send_to(buf, dst).await {
        tracing::error!(?err, "failed to send message");
        false
    } else {
        true
    }
}

#[tracing::instrument(level = Level::DEBUG, skip_all, fields(src))]
async fn handle_message(
    ctx: &mut ServerContext<'_>,
    src: SocketAddr,
    msg: ClientMessage<'_>,
    buf: &mut Vec<u8>,
) {
    macro_rules! reply_or_abort {
        ($msg:expr) => {
            if !try_send($msg, &ctx.unicast_socket, src, buf).await {
                return;
            }
        };
    }

    match msg {
        ClientMessage::Discover => {
            reply_or_abort!(ServerMessage::Announce(ctx.name));
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
                if let Err(err) = tokio::fs::create_dir_all(parent).await {
                    tracing::error!(
                        "failed to create parent directories of file: {}",
                        path.display()
                    );
                    reply_or_abort!(ServerMessage::Nack {
                        nonce,
                        msg: &format!("failed to create parent directories: {err}"),
                    });
                    return;
                }
            }
            let file = match ctx.open_opts.open(path).await {
                Ok(file) => file,
                Err(err) => {
                    tracing::error!("failed to open file: {}", path.display());
                    reply_or_abort!(ServerMessage::Nack {
                        nonce,
                        msg: &format!("failed to open file: {err}"),
                    });
                    return;
                }
            };
            // TODO: can we skip this? docs say seek past end is UB...
            if let Err(err) = file.set_len(size).await {
                tracing::error!(?err, "could not pre-allocate file");
                reply_or_abort!(ServerMessage::Nack {
                    nonce,
                    msg: &format!("failed to pre-allocate file: {err}"),
                });
                return;
            }
            let id = rand::thread_rng().gen();
            reply_or_abort!(ServerMessage::Ack { nonce, id });
            tracing::info!(%id, path = %path.display(), %hash, size, "started a new file transfer session");
            ctx.sessions.insert(
                id,
                FileEntry {
                    src,
                    file,
                    hash: hash.into_owned(),
                    curr_size: 0,
                    expected_size: size as usize,
                    received: RangeSet::new(),
                },
            );
        }
        ClientMessage::Data {
            id,
            offset,
            mut content,
        } => {
            let Some(entry) = ctx.sessions.get_mut(&id) else {
                tracing::trace!(%id, offset, len = content.len(), "ignoring chunk for unknown session");
                return;
            };
            let len = content.len();

            // should never happen
            let chunk_larger_than_expected = len > CHUNK_SIZE as usize;
            // allowed only for last chunk
            let chunk_smaller_than_expected = len < CHUNK_SIZE as usize;

            let is_last_chunk = (offset + CHUNK_SIZE) as usize >= entry.expected_size;

            if chunk_larger_than_expected || (chunk_smaller_than_expected && !is_last_chunk) {
                tracing::error!(%id, offset, len, "chunk has unexpected length");
                ctx.sessions.remove(&id);
                reply_or_abort!(ServerMessage::Error {
                    id,
                    msg: &format!("chunk {offset} has unexpected length {len}")
                });
                return;
            }

            let bytes_range = offset..=offset + (len - 1) as u64;
            // repeated messages can occur if we request a repetition but the
            // original message (or a repetition) was not lost, but just delayed
            if entry.received.contains_range(bytes_range.clone()) {
                tracing::debug!(%id, offset, "redundant chunk, skipped");
                return;
            }
            let new_size = entry.curr_size + len;
            debug_assert!(
                entry.expected_size >= new_size,
                "session {id}, at offset {offset}, \
                total bytes received ({}) exceeds expected size ({})",
                new_size,
                entry.expected_size
            );
            tracing::debug!(
                %id,
                offset,
                len,
                rem = entry.expected_size - new_size,
                "received a new chunk"
            );
            if let Err(err) = entry.file.seek(io::SeekFrom::Start(offset)).await {
                tracing::error!(?err, "could not seek file, aborting session");
                ctx.sessions.remove(&id);
                reply_or_abort!(ServerMessage::Error {
                    id,
                    msg: &format!("at chunk {offset}, error seeking: {err}")
                });
                return;
            }
            if let Err(err) = entry.file.write_all_buf(&mut content).await {
                tracing::error!(?err, "could not write chunk, aborting session");
                ctx.sessions.remove(&id);
                reply_or_abort!(ServerMessage::Error {
                    id,
                    msg: &format!("at chunk {offset}, error writing: {err}")
                });
                return;
            }
            tracing::trace!(%id, offset, "wrote chunk successfully");
            entry.curr_size = new_size;
            entry
                .received
                // we checked len > 0 previously
                .insert_range(bytes_range);
            if has_gaps(&entry.received) && ctx.request_repetitions_at.is_none() {
                // we can reach here while processing a repeated message too, in which case we may
                // have already requested a repetition for the remaining gaps as well. if we fill
                // in the gaps before the next repetition request window, that's fine, we'll detect
                // that in [`request_repetitions`] before we send any requests. note that we can
                // still end up with duplicate data messages due to delays and reordering of the
                // original messages or any repetition, so we need to handle that as well.
                tracing::debug!(
                    "gaps detected; scheduling repetition requests after {REPEAT_REQUEST_DELAY:?}"
                );
                ctx.request_repetitions_at = Some(Instant::now() + REPEAT_REQUEST_DELAY);
            }
            match entry.curr_size.cmp(&entry.expected_size) {
                Ordering::Less => {
                    tracing::trace!("remaining bytes: {}", entry.expected_size - entry.curr_size);
                }
                Ordering::Equal => {
                    // TODO: check hash
                    let entry = ctx
                        .sessions
                        .remove(&id)
                        .expect("we hold a &mut so the entry must still be there");
                    reply_or_abort!(ServerMessage::Done { id });
                    tracing::info!(%id, len = entry.curr_size, hash = %entry.hash, "file transfer completed");
                }
                Ordering::Greater => {
                    // this is technically unreachable in debug mode, but in release mode we don't
                    // run the checks prior to this point so it's good to have a fallback
                    tracing::error!(
                        recv = entry.curr_size,
                        expected = entry.expected_size,
                        "received more data than expected"
                    );
                    let entry = ctx
                        .sessions
                        .remove(&id)
                        .expect("we hold a &mut so the entry must still be there");
                    reply_or_abort!(ServerMessage::Error {
                        id,
                        msg: &format!(
                            "payload overflow: received {} bytes, expected {}",
                            entry.curr_size, entry.expected_size
                        )
                    });
                }
            }
        }
    }
}
