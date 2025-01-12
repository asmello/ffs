use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use core::fmt;
use eyre::{Context, OptionExt};
use rand::{
    distributions::{Distribution, Standard},
    Rng,
};
use std::{
    array::TryFromSliceError,
    borrow::Cow,
    io::{self, Write},
};

// assuming MTU of 1500 (typical for ethernet), this is 1500 - 40 (ipv6 header)
// - 8 (udp header) = 1452. if we estimate this too high, most networks will
// just fragment the packet, which is not the end of the world, but may increase
// re-send rate (as even one fragment lost invalidates the entire datagram).
// TODO: validate if fragmented packets are truly a problem in practice. for
// large files processing small chunks adds a ton of overhead...
// TODO: use Path MTU Discovery to dynamically adjust the MTU size. will need
// to modify protocol for variable chunk size. possibly as a prelude stage.
// https://en.wikipedia.org/wiki/Path_MTU_Discovery
pub const DATAGRAM_SIZE_LIMIT: usize = 1452;
// this is the effective capacity we have in each data message after discounting
// the metadata/header overhead (1 byte for the tag, 8 bytes for the id, 8 bytes
// for the offset), which we use as the chunk size.
pub const CHUNK_SIZE: u64 = DATAGRAM_SIZE_LIMIT as u64 - (1 + 8 + 8);

#[derive(Debug, PartialEq, Eq)]
pub struct Hash<'data>(Cow<'data, [u8; 32]>);

impl Hash<'_> {
    pub fn into_owned(self) -> Hash<'static> {
        Hash(Cow::Owned(self.0.into_owned()))
    }
}

impl TryFrom<Vec<u8>> for Hash<'static> {
    type Error = Vec<u8>;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(Cow::Owned(value.try_into()?)))
    }
}

impl fmt::Display for Hash<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0.as_ref()))
    }
}

impl From<[u8; 32]> for Hash<'static> {
    fn from(value: [u8; 32]) -> Self {
        Self(Cow::Owned(value))
    }
}

impl<'data> TryFrom<&'data [u8]> for Hash<'data> {
    type Error = TryFromSliceError;

    fn try_from(value: &'data [u8]) -> Result<Self, Self::Error> {
        Ok(Self(Cow::Borrowed(value.try_into()?)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SessionId(u32);

impl From<u32> for SessionId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}", self.0)
    }
}

impl Distribution<SessionId> for Standard {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> SessionId {
        SessionId(rng.gen())
    }
}

const DISCOVER_TAG: u8 = 0xff;
const ANNOUNCE_TAG: u8 = 0xf0;
const START_TAG: u8 = 0x00;
const ACK_TAG: u8 = 0x01;
const NACK_TAG: u8 = 0x02;
const DATA_TAG: u8 = 0x03;
const ERROR_TAG: u8 = 0x04;
const REPEAT_TAG: u8 = 0x05;
const DONE_TAG: u8 = 0x06;

#[derive(Debug)]
pub enum ServerMessage<'data> {
    Announce(&'data str),
    Ack {
        session_id: SessionId,
    },
    Nack {
        session_id: SessionId,
        msg: &'data str,
    },
    Error {
        session_id: SessionId,
        msg: &'data str,
    },
    Repeat {
        session_id: SessionId,
        offsets: Cow<'data, [u64]>,
    },
    Done {
        session_id: SessionId,
    },
}

impl<'data> ServerMessage<'data> {
    pub fn encode(&self, buf: &mut impl Write) -> io::Result<()> {
        match self {
            ServerMessage::Announce(name) => {
                buf.write_u8(ANNOUNCE_TAG)?;
                buf.write_all(name.as_bytes())?;
            }
            ServerMessage::Ack { session_id } => {
                buf.write_u8(ACK_TAG)?;
                buf.write_u32::<BigEndian>(session_id.0)?;
            }
            ServerMessage::Nack { session_id, msg } => {
                buf.write_u8(NACK_TAG)?;
                buf.write_u32::<BigEndian>(session_id.0)?;
                buf.write_all(msg.as_bytes())?;
            }
            ServerMessage::Error { session_id, msg } => {
                buf.write_u8(ERROR_TAG)?;
                buf.write_u32::<BigEndian>(session_id.0)?;
                buf.write_all(msg.as_bytes())?;
            }
            ServerMessage::Repeat {
                session_id,
                offsets,
            } => {
                buf.write_u8(REPEAT_TAG)?;
                buf.write_u32::<BigEndian>(session_id.0)?;
                for offset in offsets.as_ref() {
                    buf.write_u64::<BigEndian>(*offset)?;
                }
            }
            ServerMessage::Done { session_id } => {
                buf.write_u8(DONE_TAG)?;
                buf.write_u32::<BigEndian>(session_id.0)?;
            }
        }
        Ok(())
    }

    pub fn decode(buf: &'data [u8]) -> eyre::Result<Self> {
        let total_len = buf.len();
        let mut cursor = io::Cursor::new(buf);
        match cursor.read_u8().wrap_err("missing tag")? {
            ANNOUNCE_TAG => Ok(Self::Announce(std::str::from_utf8(remaining_slice(
                cursor,
            ))?)),
            ACK_TAG => {
                let session_id = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete ack message, could not read session")?
                    .into();
                Ok(Self::Ack { session_id })
            }
            NACK_TAG => {
                let session = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete nack message, could not read session")?
                    .into();
                let msg = std::str::from_utf8(remaining_slice(cursor))?;
                Ok(Self::Nack {
                    session_id: session,
                    msg,
                })
            }
            ERROR_TAG => {
                let session_id = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete error message, could not read id")?
                    .into();
                let msg = std::str::from_utf8(remaining_slice(cursor))?;
                Ok(Self::Error { session_id, msg })
            }
            REPEAT_TAG => {
                let session_id = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete repeat message, could not read id")?
                    .into();
                // total_len = 1 byte (tag) + 8 bytes (id) + offsets_len so
                // offsets_len = total_len - 1 - 8 and since each offset is 8
                // bytes, offsets_len / 8 gives us the offset_count
                let offset_count = (total_len - 1 - 8) / 8;
                let mut offsets = Vec::with_capacity(offset_count);
                for _ in 0..offset_count {
                    offsets.push(
                        cursor
                            .read_u64::<BigEndian>()
                            .expect("count was derived from len"),
                    );
                }
                // note we can't just use the original buffer directly because
                // we need 8 bytes alignment, and the buffer is only 1 byte
                // aligned
                // TODO: can we make the message 8 bytes aligned? would require
                // some padding but might be worth avoiding the extra allocation
                // and copying
                Ok(Self::Repeat {
                    session_id,
                    offsets: Cow::Owned(offsets),
                })
            }
            DONE_TAG => {
                let session_id = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete done message, could not read id")?
                    .into();
                Ok(Self::Done { session_id })
            }
            other => eyre::bail!("unexpected tag: {other:#02x}"),
        }
    }
}

fn remaining_slice(cursor: io::Cursor<&[u8]>) -> &[u8] {
    let pos = cursor.position();
    let slice = cursor.into_inner();
    &slice[pos as usize..]
}

#[derive(Debug)]
pub enum ClientMessage<'data> {
    Discover,
    Start {
        session_id: SessionId,
        size: u64,
        hash: Hash<'data>,
        path: &'data str,
    },
    Data {
        session_id: SessionId,
        offset: u64,
        content: &'data [u8],
    },
}

impl<'data> ClientMessage<'data> {
    pub fn encode(&self, buf: &mut impl Write) -> io::Result<()> {
        match self {
            ClientMessage::Discover => {
                buf.write_u8(DISCOVER_TAG)?;
            }
            ClientMessage::Start {
                session_id: session,
                size,
                hash,
                path,
            } => {
                buf.write_u8(START_TAG)?;
                buf.write_u32::<BigEndian>(session.0)?;
                buf.write_u64::<BigEndian>(*size)?;
                buf.write_all(hash.0.as_ref())?;
                buf.write_all(path.as_bytes())?;
            }
            ClientMessage::Data {
                session_id,
                offset,
                content,
            } => {
                buf.write_u8(DATA_TAG)?;
                buf.write_u32::<BigEndian>(session_id.0)?;
                buf.write_u64::<BigEndian>(*offset)?;
                buf.write_all(content)?;
            }
        }
        Ok(())
    }

    pub async fn encode_data_msg(
        id: SessionId,
        offset: u64,
        content: impl tokio::io::AsyncRead,
        buf: &mut Vec<u8>,
    ) -> io::Result<usize> {
        buf.write_u8(DATA_TAG)?;
        buf.write_u32::<BigEndian>(id.0)?;
        buf.write_u64::<BigEndian>(offset)?;

        use tokio::io::AsyncReadExt;
        tokio::pin!(content);
        // :sigh: so read_buf gives no guarantees about whether it will fill up the
        // buffer or not. it often doesn't, in practice. we could use `read_exact`
        // instead, but then there are no guarantees about what happens if the EOF
        // is reached while attempting to fill up the buffer. so to be on the safe
        // side, we fill up the buffer manually until either the buffer is filled or
        // the file runs out of bytes.
        // TODO: synchronous reads might be faster...
        let mut read = 0;
        loop {
            let n = content.read_buf(buf).await?;
            read += n;
            if buf.len() == buf.capacity() || n == 0 {
                break Ok(read);
            }
        }
    }

    pub fn encode_data_msg_sync(
        id: SessionId,
        offset: u64,
        mut content: impl std::io::Read,
        buf: &mut Vec<u8>,
    ) -> io::Result<usize> {
        buf.write_u8(DATA_TAG)?;
        buf.write_u32::<BigEndian>(id.0)?;
        buf.write_u64::<BigEndian>(offset)?;

        // 1 + 8 + 8 from header
        let mut total = 17;
        loop {
            let read = content.read(buf)?;
            total += read;
            if buf.len() == buf.capacity() || read == 0 {
                break Ok(total);
            }
        }
    }

    pub fn decode(buf: &'data [u8]) -> eyre::Result<Self> {
        let mut cursor = io::Cursor::new(buf);
        match cursor.read_u8().wrap_err("empty payload")? {
            DISCOVER_TAG => Ok(Self::Discover),
            START_TAG => {
                let session_id = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete start message, could not read session")?
                    .into();
                let size = cursor
                    .read_u64::<BigEndian>()
                    .wrap_err("incomplete start message, could not read size")?;
                let (hash, path) = remaining_slice(cursor)
                    .split_at_checked(32)
                    .ok_or_eyre("incomplete start message, could not read hash")?;
                let hash = hash.try_into()?;
                let path = std::str::from_utf8(path)?;
                eyre::ensure!(!path.is_empty(), "path must not be empty");
                Ok(Self::Start {
                    session_id,
                    size,
                    hash,
                    path,
                })
            }
            DATA_TAG => {
                let session_id = cursor
                    .read_u32::<BigEndian>()
                    .wrap_err("incomplete data message, could not read id")?
                    .into();
                let offset = cursor
                    .read_u64::<BigEndian>()
                    .wrap_err("incomplete data message, could not read offset")?;
                let content = remaining_slice(cursor);
                Ok(Self::Data {
                    session_id,
                    offset,
                    content,
                })
            }
            other => eyre::bail!("unexpected tag: {other:#02x}"),
        }
    }
}
