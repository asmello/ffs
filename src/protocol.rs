use bitcode::{Decode, Encode};

#[derive(Encode, Decode)]
pub enum Message<'data> {
    Discover,
    Announce(&'data str),
    Start {
        nonce: u32,
        size: u64,
        hash: [u8; 16],
        path: &'data str,
    },
    Ack {
        nonce: u32,
        id: u64,
    },
    Nack {
        nonce: u32,
        msg: &'data str,
    },
    Data {
        id: u64,
        chunk: u64,
        content: Vec<u8>,
    },
    Error {
        id: u64,
        msg: &'data str,
    },
    Repeat {
        id: u64,
        chunks: Vec<u64>,
    },
    Done {
        id: u64,
    },
}
