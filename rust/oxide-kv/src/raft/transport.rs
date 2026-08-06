//! Multiplexed transport for inter-node RPC.
//!
//! As of P6, the cluster's single TCP listener (the one bound to
//! `Config::listen_addr` in `main.rs`) carries **two** RPC surfaces:
//!
//!   - `0x01` — Raft consensus RPCs (RequestVote, AppendEntries,
//!     InstallSnapshot, replies). These continue to flow through the
//!     existing `RpcServer::handle_raft_rpc` path.
//!   - `0x02` — 2PC coordinator vote RPC (`VoteRequest` /
//!     `VoteResponse`). New in P6 PR #12; handled by
//!     `RpcServer::handle_vote_rpc`.
//!
//! Wire format (every request and reply on the shared port):
//!
//! ```text
//! +------+----------------+-------------------+
//! | kind | length (u32 BE) | protobuf payload  |
//! | u8   | 4 bytes        | `length` bytes    |
//! +------+----------------+-------------------+
//! ```
//!
//! `kind` discriminates which handler should consume the frame. `length`
//! is the size of the protobuf payload that follows; the framed payload
//! is opaque bytes to this module and is decoded by the per-RPC layer
//! (`raft::proto::pb::RaftMessage` for Raft RPCs,
//! `coordination::pb::VoteRequest` / `VoteResponse` for the 2PC RPC).
//!
//! ## Why multiplex on the existing port
//!
//! The locked decision for P6 (see `ROADMAP.md` and PR #12 spec) is to
//! reuse the existing Raft TCP listener rather than opening a second
//! port for the coordinator vote RPC. A second port complicates
//! firewall and container-deployment surfaces (two ports per node
//! instead of one) and buys nothing — vote RPCs are infrequent and
//! small, so they ride alongside heartbeats without contention. The
//! discriminator byte is the cost of staying on a single port.
//!
//! ## Backward compatibility
//!
//! The 1-byte discriminator is a **breaking** wire change relative to
//! the pre-P6 Raft frame (which started straight with the 4-byte
//! length prefix). All in-flight deployments, if any, would need a
//! coordinated upgrade. As of this PR there are no deployed peers
//! (P6 is the first phase to exercise multi-node behavior), so the
//! cutover is safe.

use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Protocol discriminator sent as the first byte of every request /
/// reply on the shared inter-node port.
///
/// `0x01` carries the Raft consensus surface; `0x02` carries the 2PC
/// coordinator vote surface. `0x00` and any unknown value are reserved
/// for future use; the receiver must treat them as a protocol error
/// rather than silently misinterpret the frame.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchKind {
    Raft = 0x01,
    Vote = 0x02,
}

impl DispatchKind {
    /// Decode a byte into a known discriminator. Returns `None` for
    /// reserved or unknown values so the caller can surface a clear
    /// protocol error instead of misrouting the frame.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Raft),
            0x02 => Some(Self::Vote),
            _ => None,
        }
    }
}

/// Maximum payload size accepted on the multiplexed port.
///
/// 16 MiB is well above any plausible single Raft RPC (a snapshot is
/// the largest, and 4 MiB memtable flushes dominate there) or vote
/// request (a handful of bytes plus a string `tx_id`). Larger frames
/// are rejected at the framing layer rather than risking unbounded
/// memory allocation from a hostile or buggy peer.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Write a single multiplexed frame: discriminator, length prefix,
/// then payload. `writer` is flushed before returning.
pub(crate) async fn write_envelope<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    kind: DispatchKind,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame payload {} bytes exceeds MAX_FRAME_BYTES ({})",
                payload.len(),
                MAX_FRAME_BYTES
            ),
        ));
    }
    writer.write_all(&[kind as u8]).await?;
    writer.write_all(&(payload.len() as u32).to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a single multiplexed frame from `reader`. Returns
/// `(DispatchKind, payload_bytes)` on success.
///
/// Returns `Ok(None)` on clean EOF (peer closed the socket before
/// sending any bytes). On any partial / invalid frame, returns an
/// `io::Error` so the caller can log and drop the connection rather
/// than risk corrupting subsequent reads.
pub(crate) async fn read_envelope<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Option<(DispatchKind, Vec<u8>)>> {
    let mut kind_buf = [0u8; 1];
    match reader.read_exact(&mut kind_buf).await {
        Ok(0) => return Ok(None), // EOF before any byte
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let kind = DispatchKind::from_byte(kind_buf[0]).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown protocol discriminator 0x{:02x}", kind_buf[0]),
        )
    })?;

    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame length {} exceeds MAX_FRAME_BYTES ({})",
                len, MAX_FRAME_BYTES
            ),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some((kind, payload)))
}

/// Read only the discriminator byte from a multiplexed stream.
///
/// Server-side helper used by the dispatch loop: callers want to
/// route on the discriminator without consuming the rest of the
/// frame. After this returns, the next bytes on the stream are the
/// 4-byte length prefix and the protobuf payload — i.e. the
/// `read_envelope_payload` helper below.
///
/// Returns `Ok(None)` on clean EOF before any byte arrives.
pub(crate) async fn read_envelope_discriminator<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Option<DispatchKind>> {
    let mut kind_buf = [0u8; 1];
    match reader.read_exact(&mut kind_buf).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    DispatchKind::from_byte(kind_buf[0]).map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown protocol discriminator 0x{:02x}", kind_buf[0]),
        )
    })
}

/// Read the length-prefixed payload portion of a frame after
/// `read_envelope_discriminator` has already consumed the kind byte.
///
/// Returns `Ok(None)` on clean EOF before any length bytes arrive.
pub(crate) async fn read_envelope_payload<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame length {} exceeds MAX_FRAME_BYTES ({})",
                len, MAX_FRAME_BYTES
            ),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn envelope_roundtrip_raft_kind() {
        let payload = b"some-raft-rpc-payload";
        let (mut a, mut b) = duplex(64 * 1024);
        write_envelope(&mut a, DispatchKind::Raft, payload).await.unwrap();
        let (kind, got) = read_envelope(&mut b).await.unwrap().unwrap();
        assert_eq!(kind, DispatchKind::Raft);
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn envelope_roundtrip_vote_kind() {
        let payload = b"some-vote-payload";
        let (mut a, mut b) = duplex(64 * 1024);
        write_envelope(&mut a, DispatchKind::Vote, payload).await.unwrap();
        let (kind, got) = read_envelope(&mut b).await.unwrap().unwrap();
        assert_eq!(kind, DispatchKind::Vote);
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn envelope_roundtrip_large_payload() {
        // Force the 4-byte length prefix beyond one byte so the BE
        // encoding is actually exercised.
        let payload: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();
        let (mut a, mut b) = duplex(256 * 1024);
        write_envelope(&mut a, DispatchKind::Vote, &payload).await.unwrap();
        let (kind, got) = read_envelope(&mut b).await.unwrap().unwrap();
        assert_eq!(kind, DispatchKind::Vote);
        assert_eq!(got.len(), payload.len());
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn envelope_returns_none_on_eof_before_any_byte() {
        let (a, mut b) = duplex(16);
        drop(a);
        let got = read_envelope(&mut b).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn envelope_rejects_unknown_discriminator() {
        let (mut a, mut b) = duplex(64);
        // Send 0xFF as the discriminator, then a valid 4-byte length + payload.
        a.write_all(&[0xFFu8]).await.unwrap();
        a.write_all(&4u32.to_be_bytes()).await.unwrap();
        a.write_all(b"abcd").await.unwrap();
        drop(a);

        let err = read_envelope(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(format!("{}", err).contains("0xff"));
    }

    #[tokio::test]
    async fn envelope_rejects_oversized_length() {
        // Length field claims MAX_FRAME_BYTES+1; payload is shorter.
        // The reader should reject before attempting the allocation.
        let bad_len = (MAX_FRAME_BYTES as u32 + 1).to_be_bytes();
        let (mut a, mut b) = duplex(64);
        a.write_all(&[DispatchKind::Raft as u8]).await.unwrap();
        a.write_all(&bad_len).await.unwrap();
        a.write_all(b"short").await.unwrap();
        drop(a);

        let err = read_envelope(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(format!("{}", err).contains("exceeds MAX_FRAME_BYTES"));
    }

    #[tokio::test]
    async fn envelope_rejects_zero_length_discriminator() {
        let (mut a, mut b) = duplex(64);
        a.write_all(&[0x00u8]).await.unwrap();
        a.write_all(&0u32.to_be_bytes()).await.unwrap();
        drop(a);

        let err = read_envelope(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn dispatch_kind_from_byte_known_values() {
        assert_eq!(DispatchKind::from_byte(0x01), Some(DispatchKind::Raft));
        assert_eq!(DispatchKind::from_byte(0x02), Some(DispatchKind::Vote));
    }

    #[test]
    fn dispatch_kind_from_byte_rejects_unknown() {
        assert_eq!(DispatchKind::from_byte(0x00), None);
        assert_eq!(DispatchKind::from_byte(0x03), None);
        assert_eq!(DispatchKind::from_byte(0xFF), None);
    }
}