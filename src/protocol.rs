use bitcode::{Decode, Encode};

pub type Hash = [u8; 16];

#[derive(Debug, Encode, Decode)]
pub enum ServerMessage<'data> {
    Announce(&'data str),
    Ack { nonce: u32, id: u64 },
    Nack { nonce: u32, msg: &'data str },
    Error { id: u64, msg: &'data str },
    Repeat { id: u64, chunks: Vec<u64> },
    Done { id: u64 },
}

#[derive(Debug, Encode, Decode)]
pub enum ClientMessage<'data> {
    Discover,
    Start {
        nonce: u32,
        size: u64,
        hash: Hash,
        path: &'data str,
    },
    Data {
        id: u64,
        chunk: u64,
        content: Vec<u8>,
    },
}
