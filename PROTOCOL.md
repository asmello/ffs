# Fast File Share Protocol
This is a UDP-based, push-driven file sharing protocol. It is optimised for speed and simplicity, not security, and is meant to operate within internal trusted networks, such as domestic LANs. As such, features like encryption and authentication are explicitly excluded from scope. If desired, more advanced functionality can be obtained from the many existing file transfer protocols.

## Motivation

A common workflow for domestic users of computers is to transfer a file from one machine to another. This is generally straightforward if both source and target systems use the same operating system, but becomes much less accessible in heterogeneous networks. There are a few protocols that can bridge the gap by offering ubiquitous client and server implementations, such as SFTP, but they tend to provide sub-optimal user friendliness or performance.

This is simple to understand. These options are so widely available because they have been around for decades, and have evolved organically to support a wide variety of use cases. This means they carry a lot of baggage that makes the default experience under-utilise modern resources, and configuring them for optimal performance takes expertise, if it is even possible. By foregoing compatibility with legacy systems and focusing on the domestic use case, however, one can do much better.

## Audience

This protocol is currently experimental, so it is not meant for public use. Once stabilised that may change.

## Goal

This protocol aims to implement single-file transfers from a source to one to one target system.

### Constraints

- Multiple files can be transferred concurrently between the same pair of systems.
    - This allows the client to implement batch transfers simply by running multiple instances of the protocol.
- Neither system has visibility into their counterpart’s filesystem.
    - This means the source system cannot choose where the transferred file will land.
    - This means the transfer must always be initiated by the source system.
- File integrity must be preserved, as well as basic metadata.
    - The initial version of this protocol only concerns itself with preserving the file name. Future iterations may extend this to other metadata.

# Definitions

*Source system*: the machine that wishes to transfer a file to the target system.

*Target system*: the machine that will received a file from the source system.

*Client*: the implementation of this protocol that runs in the source system.

*Server*: the implementation of this protocol that runs in the target system.

# Specification

## Network Layer

This protocol runs atop the UDP protocol. While we must ensure that each bit of the file is eventually delivered, we don’t need all of TCP’s guarantees about orderly delivery and congestion control, so this choice allows us to keep the protocol slim and fast.

Semantically, in this protocol, each datagram represents a self-contained message.

## Messages

All messages are prefixed with the magic number `0xff53`, and a single byte representing the protocol version. This document describes version `0x00`.

Following that prefix is an Opcode that acts to implicitly synchronise state between client and server. This also occupies a single byte. The remainder of the message structure depends on the Opcode.

## Opcodes

- `DISCOVER` / `0xff`
    - Originator: source
    - Description: used to discover available servers.
- `ANNOUNCE` / `0xf0`
    - Originator: target
    - Description: used to announce the availability of a server.
    - Arguments
        - `NAME` (arbitrary) - a UTF8 string identifying the server. Suggested to be equal to the hostname.
- `START` / `0x00`
    - Originator: source
    - Description: used to initiate a file transfer session.
    - Arguments
        - `NONCE` (4 bytes) - identifier for this file transfer. Arbitrary and chosen by the client. The client must ensure it doesn’t clash with any nonces used by concurrent transfers. It is recommended that the server reject a duplicated session if one is detected.
        - `SIZE` (8 bytes) - size of the file to be transferred in bytes.
        - `HASH` (32 bytes) - SHA256 of the file to be transferred.
        - `PATH` (arbitrary) - name or path of the file, in UTF8 encoding. If path, it must be UNIX style with forward slashes (`/`) as separators. The path must not start with a slash, and is interpreted to be relative to the root folder set by the target system (the client has no visibility of where this actually is). The size is inferred from the total datagram size.
- `ACK` / `0x01`
    - Originator: target
    - Description: used to acknowledge the session, and provide new session identifier chosen by the server.
    - Arguments
        - `NONCE` (4 bytes) - the same identifier provided by the client to initiate the session.
        - `ID` (8 bytes) - new identifier for the session, chosen by the server. Must be globally unique across all transfers the server is engaged with. All subsequent communications within the session must use this identifier as the first argument.
- `NACK` / `0x02`
    - Originator: source or target
    - Description: used to reject the session, and provide a reason.
    - Arguments
        - `NONCE` (4 bytes) - the same identifier provided by the client to initiate the session.
        - `MSG` (arbitrary) - server generated string in UTF-8 encoding representing the reason the server rejected the session. Future extensions may provide structure to this field, but currently it is left opaque.
- `DATA` / `0x03`
    - Originator: source
    - Description: used to send a chunk of data from the file.
    - Arguments
        - `ID` (8 bytes) - session identifier chosen by the server.
        - `CHUNK` (8 bytes) - start offset of the chunk of data, in bytes.
        - `CONTENT` (arbitrary) - raw data from the file.
- `ERROR` / `0x04`
    - Originator: source or target
    - Description: used by the one party to signal an error condition to the other. Terminates the session.
    - Arguments
        - `ID` (8 bytes) - session identifier chosen by the server.
        - `MSG` (arbitrary) - implementation generated string in UTF-8 encoding representing the error that occurred. Future extensions may provide structure to this field, but currently it is left opaque.
- `REPEAT` / `0x05`
    - Originator: target
    - Description: used by the server to request retransmission of one or more chunks.
    - Arguments
        - `ID` (8 bytes) - session identifier chosen by the server.
        - `CHUNKS` (arbitrary, multiple of 8) - offsets of the missing chunks.
- `DONE` / `0x06`
    - Originator: target
    - Description: used by the server to indicate all chunks were correctly received and validated.
    - Arguments
        - `ID` (8 bytes) - session identifier chosen by the server.

## Description

Before a transfer can start, a discovery sub-protocol must take place. The client broadcasts a `DISCOVER` message to the network, to which every active server must reply with an `ANNOUNCE`. Servers may also periodically broadcast `ANNOUNCE` messages as an alternate method of discovery, so clients should be ready to process those at any time asynchronously.

Once servers are known, the client may then initiate a transfer with one of them. Future versions of this protocol may allow for broadcasting files to multiple servers simultaneously, but this use case is currently excluded from scope.

The protocol starts when the client sends the server a `START` message. This message implicitly tells the server the address of the client, which is used thereafter for communications. The server then either responds with an `ACK` message, or with a `NACK`. If the client receives a `NACK`, it must stop communicating (and the server should ignore any messages it receives afterwards). In case of an `ACK`, the client initiates data transmission.

Transmission is done by number of `DATA` messages sent to the server, in arbitrary order and possibly concurrently. The server knows how to reconstruct the file from its chunks because it knows the total expected size, and each chunk specifies its offset. Once the server detects it stopped receiving messages from the client, if any chunks are missing, it then responds with a `REPEAT` message, and the cycle continues until all chunks have been received. At that point, the server responds with `DONE`, ending the session.

If, at any point, either the server or the client run into an error condition, they must report the error with an `ERROR` message, and stop communications immediately. Error reporting is best effort only, and is meant to both provide some context as well as trigger an early end to the session for best user experience, but in the case of prolonged period of silence from the other party, implementations are allowed to time out as an escape hatch. Time out events should also be reported as errors.

## Example flows

**Success**

```
> START [NONCE] [SIZE] [HASH] [PATH]
< ACK [NONCE] [ID]
> DATA [ID] 0 [DATA0]
> DATA [ID] 65507 [DATA1]
...
< MISS [ID] 65507 262028 ...
> DATA [ID] 65507 [DATA1]
...
< DONE [ID]
```

**Rejected**

```
> START [NONCE] [SIZE] [HASH] [PATH]
< NACK [NONCE] [MSG]
```

**Error**

```
> START [NONCE] [SIZE] [HASH] [PATH]
< ACK [NONCE] [ID]
> DATA [ID] 0 [DATA0]
> DATA [ID] 65507 [DATA1]
...
< ERROR [ID] "Insufficient space."
> DATA [ID] 262028 [DATA4]
< ERROR [ID] "Insufficient space."
```
