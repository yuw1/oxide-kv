//! 2PC coordinator wire types.
//!
//! Generated protobuf types for the 2PC coordinator side-channel RPC
//! (see `proto/coordination.proto`), plus domain types and `From`
//! conversions.
//!
//! Scope of this module (PR #11 of P6, see ROADMAP.md):
//!   - Wire-format types and encoding round-trip.
//!   - Plain domain structs with no business logic.
//!
//! Scope deferred to PR #12+:
//!   - RPC handler dispatch on the receiving node.
//!   - Quorum policy / timeout decisions (see `proto/coordination.proto`
//!     header — locked to **all-yes required** for P6).
//!   - Transport choice (separate port vs. multiplexed socket).
//!
//! Background — why coordinator votes aren't a Raft log entry:
//!
//! In the chosen Option A design (see ROADMAP.md), the Raft log carries
//! only `BeginTx` and `DecideTx` (defined in `proto/raft.proto`). Between
//! those two entries, the leader (acting as the 2PC coordinator) asks
//! every peer "is this transaction safe to commit?" out-of-band via the
//! `VoteRequest` / `VoteResponse` messages defined here. This keeps vote
//! collection — a coordinator concern — out of the consensus log where
//! it would otherwise inflate log size per tx and confuse log readers.
//!
//! Wire framing is length-prefixed protobuf, identical to Raft (4-byte
//! big-endian length prefix followed by the encoded message). PR #12
//! will introduce a `CoordinationMessage` envelope or a dedicated port;
//! that choice is intentionally not made here.

// Pull in the prost-generated module. The file path is fixed by
// `prost-build` based on the `package` declaration in
// `proto/coordination.proto`.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/oxide_kv.coordination.rs"));
}

/// Domain type for the leader → peer "please vote on this tx" request.
///
/// Fields mirror the generated `pb::VoteRequest` exactly. The domain
/// type is plain Rust so PR #12 can attach handler-side validation
/// without leaking protobuf types into the application code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteRequest {
    pub term: u64,
    pub tx_id: String,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

/// Domain type for the peer → leader "here is my vote" response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteResponse {
    pub term: u64,
    pub vote_granted: bool,
    /// Short human-readable diagnostic populated when `vote_granted = false`
    /// (e.g. "tx not pending", "conflict on key k"). Empty when granted.
    pub reason: String,
}

// =========================================================================
// Domain → proto
// =========================================================================

impl From<&VoteRequest> for pb::VoteRequest {
    fn from(d: &VoteRequest) -> Self {
        pb::VoteRequest {
            term: d.term,
            tx_id: d.tx_id.clone(),
            last_log_index: d.last_log_index,
            last_log_term: d.last_log_term,
        }
    }
}

impl From<&VoteResponse> for pb::VoteResponse {
    fn from(d: &VoteResponse) -> Self {
        pb::VoteResponse {
            term: d.term,
            vote_granted: d.vote_granted,
            reason: d.reason.clone(),
        }
    }
}

// =========================================================================
// Proto → domain
// =========================================================================

impl From<pb::VoteRequest> for VoteRequest {
    fn from(p: pb::VoteRequest) -> Self {
        VoteRequest {
            term: p.term,
            tx_id: p.tx_id,
            last_log_index: p.last_log_index,
            last_log_term: p.last_log_term,
        }
    }
}

impl From<pb::VoteResponse> for VoteResponse {
    fn from(p: pb::VoteResponse) -> Self {
        VoteResponse {
            term: p.term,
            vote_granted: p.vote_granted,
            reason: p.reason,
        }
    }
}

// =========================================================================
// Tests
// =========================================================================
//
// All tests live under the same module so a single `cargo test coordination`
// runs them in one pass.
//
// Coverage matrix (matches PR #11 spec acceptance items):
//   - Round-trip encode/decode for both messages.
//   - Boundary values: term = 0, tx_id = "", last_log_index = u64::MAX.
//   - Forward compatibility: a reader compiled against an older schema
//     (fewer fields) must still successfully decode a payload written
//     by the current schema. Simulated by hand-crafting a proto payload
//     with the new `reason` field and decoding it via the old field set.
//
// We exercise the conversions both via `From` (typed round-trip) and via
// `prost::Message::encode_length_delimited` / `decode_length_delimited`
// (wire-format round-trip) so a regression in either layer is caught.

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    // -- typed round-trip -------------------------------------------------

    #[test]
    fn vote_request_domain_to_proto_round_trip() {
        let d = VoteRequest {
            term: 42,
            tx_id: "tx-1".to_string(),
            last_log_index: 17,
            last_log_term: 41,
        };
        let pb: pb::VoteRequest = (&d).into();
        let back: VoteRequest = pb.into();
        assert_eq!(d, back);
    }

    #[test]
    fn vote_response_domain_to_proto_round_trip() {
        let d = VoteResponse {
            term: 42,
            vote_granted: true,
            reason: String::new(),
        };
        let pb: pb::VoteResponse = (&d).into();
        let back: VoteResponse = pb.into();
        assert_eq!(d, back);
    }

    // -- wire-format round-trip ------------------------------------------

    #[test]
    fn vote_request_wire_round_trip() {
        let d = VoteRequest {
            term: 7,
            tx_id: "tx-wire".to_string(),
            last_log_index: 99,
            last_log_term: 6,
        };
        let pb: pb::VoteRequest = (&d).into();
        let bytes = pb.encode_length_delimited_to_vec();
        let decoded =
            pb::VoteRequest::decode_length_delimited(bytes.as_slice()).expect("decode VoteRequest");
        let back: VoteRequest = decoded.into();
        assert_eq!(d, back);
    }

    #[test]
    fn vote_response_wire_round_trip_with_reason() {
        let d = VoteResponse {
            term: 3,
            vote_granted: false,
            reason: "tx not pending on this node".to_string(),
        };
        let pb: pb::VoteResponse = (&d).into();
        let bytes = pb.encode_length_delimited_to_vec();
        let decoded =
            pb::VoteResponse::decode_length_delimited(bytes.as_slice()).expect("decode VoteResponse");
        let back: VoteResponse = decoded.into();
        assert_eq!(d, back);
    }

    // -- boundary values --------------------------------------------------

    #[test]
    fn vote_request_boundary_term_zero_tx_id_empty() {
        let d = VoteRequest {
            term: 0,
            tx_id: String::new(),
            last_log_index: 0,
            last_log_term: 0,
        };
        let pb: pb::VoteRequest = (&d).into();
        let bytes = pb.encode_length_delimited_to_vec();
        let decoded = pb::VoteRequest::decode_length_delimited(bytes.as_slice())
            .expect("decode boundary VoteRequest");
        let back: VoteRequest = decoded.into();
        assert_eq!(d, back);
    }

    #[test]
    fn vote_request_boundary_last_log_index_u64_max() {
        // The proto field is `uint64`, so this should encode/decode without
        // truncation. Documents the upper-bound behavior for future readers.
        let d = VoteRequest {
            term: 1,
            tx_id: "tx-big".to_string(),
            last_log_index: u64::MAX,
            last_log_term: u64::MAX,
        };
        let pb: pb::VoteRequest = (&d).into();
        let bytes = pb.encode_length_delimited_to_vec();
        let decoded = pb::VoteRequest::decode_length_delimited(bytes.as_slice())
            .expect("decode max VoteRequest");
        assert_eq!(decoded.last_log_index, u64::MAX);
        assert_eq!(decoded.last_log_term, u64::MAX);
        let back: VoteRequest = decoded.into();
        assert_eq!(d, back);
    }

    #[test]
    fn vote_response_boundary_empty_reason() {
        // Reason is intentionally empty on a granted vote; verify empty
        // string survives encode/decode (prost defaults to "" on absent
        // string fields, which we should not silently change).
        let d = VoteResponse {
            term: 0,
            vote_granted: true,
            reason: String::new(),
        };
        let pb: pb::VoteResponse = (&d).into();
        let bytes = pb.encode_length_delimited_to_vec();
        let decoded = pb::VoteResponse::decode_length_delimited(bytes.as_slice())
            .expect("decode empty-reason VoteResponse");
        assert_eq!(decoded.reason, "");
        assert!(decoded.vote_granted);
        let back: VoteResponse = decoded.into();
        assert_eq!(d, back);
    }

    // -- forward compatibility -------------------------------------------
    //
    // Spec item 3 (forward compat): an older reader (compiled against a
    // schema with fewer fields) must still be able to decode a payload
    // written by the current schema. The proto3 wire format reserves
    // field tags, so unknown fields are preserved in the unknown-field
    // set but do not break decoding of known ones.
    //
    // We simulate this by hand-encoding a payload that contains an extra
    // field beyond the current schema (a varint tag > 16, which requires
    // multi-byte varint encoding), then decoding it with the current
    // reader. The current reader should ignore the unknown tag and
    // produce the expected values for known fields.

    /// Encode a u32 protobuf field tag (field_num << 3 | wire_type) as
    /// its varint form. Used to build a hand-crafted unknown-tag payload.
    fn encode_tag(field_num: u32, wire_type: u32) -> Vec<u8> {
        let mut v: u64 = ((field_num as u64) << 3) | (wire_type as u64);
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            } else {
                out.push(byte | 0x80);
            }
        }
        out
    }

    /// Encode a u64 as a protobuf varint.
    fn encode_varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            } else {
                out.push(byte | 0x80);
            }
        }
        out
    }

    #[test]
    fn vote_response_forward_compat_unknown_field_ignored() {
        // Build a payload with the three current fields, plus a synthetic
        // unknown field at tag 99 (wire-type varint, value 12345). Tag 99
        // forces multi-byte varint encoding for the tag itself; this
        // proves the decoder correctly walks past it.
        let mut buf = Vec::new();
        // tag 1 (term) varint 5
        buf.extend_from_slice(&encode_tag(1, 0));
        buf.extend_from_slice(&encode_varint(5));
        // tag 2 (vote_granted) varint 1
        buf.extend_from_slice(&encode_tag(2, 0));
        buf.extend_from_slice(&encode_varint(1));
        // tag 3 (reason) len-delim string ""
        buf.extend_from_slice(&encode_tag(3, 2));
        buf.extend_from_slice(&encode_varint(0));
        // tag 99 unknown varint 12345
        buf.extend_from_slice(&encode_tag(99, 0));
        buf.extend_from_slice(&encode_varint(12345));

        let decoded = pb::VoteResponse::decode(buf.as_slice())
            .expect("decode payload with unknown field tag 99");
        assert_eq!(decoded.term, 5);
        assert!(decoded.vote_granted);
        assert_eq!(decoded.reason, "");
        let back: VoteResponse = decoded.into();
        assert_eq!(back.term, 5);
        assert!(back.vote_granted);
        assert_eq!(back.reason, "");
    }

    #[test]
    fn vote_request_forward_compat_unknown_field_ignored() {
        // Same idea for VoteRequest: payload with the four current fields
        // and an unknown tag-99 varint at the tail.
        let mut buf = Vec::new();
        // tag 1 (term) varint 1
        buf.extend_from_slice(&encode_tag(1, 0));
        buf.extend_from_slice(&encode_varint(1));
        // tag 2 (tx_id) len-delim "tx-fwd"
        buf.extend_from_slice(&encode_tag(2, 2));
        buf.extend_from_slice(&encode_varint(6));
        buf.extend_from_slice(b"tx-fwd");
        // tag 3 (last_log_index) varint 10
        buf.extend_from_slice(&encode_tag(3, 0));
        buf.extend_from_slice(&encode_varint(10));
        // tag 4 (last_log_term) varint 1
        buf.extend_from_slice(&encode_tag(4, 0));
        buf.extend_from_slice(&encode_varint(1));
        // tag 99 unknown varint 12345
        buf.extend_from_slice(&encode_tag(99, 0));
        buf.extend_from_slice(&encode_varint(12345));

        let decoded = pb::VoteRequest::decode(buf.as_slice())
            .expect("decode payload with unknown field tag 99");
        assert_eq!(decoded.term, 1);
        assert_eq!(decoded.tx_id, "tx-fwd");
        assert_eq!(decoded.last_log_index, 10);
        assert_eq!(decoded.last_log_term, 1);
        let back: VoteRequest = decoded.into();
        assert_eq!(back.tx_id, "tx-fwd");
        assert_eq!(back.last_log_index, 10);
    }
}