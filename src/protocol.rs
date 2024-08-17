use bitcode::{Decode, Encode};
use core::fmt;
use rand::{
    distributions::{Distribution, Standard},
    Rng,
};

#[derive(Debug, Encode, Decode)]
pub struct Hash([u8; 32]);

impl TryFrom<Vec<u8>> for Hash {
    type Error = Vec<u8>;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        Ok(Self(value.try_into()?))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

impl From<[u8; 32]> for Hash {
    fn from(value: [u8; 32]) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Decode, Encode)]
#[repr(transparent)]
pub struct Identifier(u64);

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl Distribution<Identifier> for Standard {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Identifier {
        Identifier(rng.gen())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Decode, Encode)]
#[repr(transparent)]
pub struct Nonce(u32);

impl fmt::Display for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}", self.0)
    }
}

impl Distribution<Nonce> for Standard {
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> Nonce {
        Nonce(rng.gen())
    }
}

#[derive(Debug, Encode, Decode)]
pub enum ServerMessage<'data> {
    Announce(&'data str),
    Ack { nonce: Nonce, id: Identifier },
    Nack { nonce: Nonce, msg: &'data str },
    Error { id: Identifier, msg: &'data str },
    Repeat { id: Identifier, offsets: Vec<u64> },
    Done { id: Identifier },
}

#[derive(Debug, Encode, Decode)]
pub enum ClientMessage<'data> {
    Discover,
    Start {
        nonce: Nonce,
        size: u64,
        hash: Hash,
        path: &'data str,
    },
    Data {
        id: Identifier,
        offset: u64,
        content: Vec<u8>,
    },
}
