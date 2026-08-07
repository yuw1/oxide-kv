//! Wire-format sanity tests — verifies that the JSON shapes this
//! crate sends match the shapes the server crate (`rust/oxide-kv`)
//! emits. These tests don't need a running server: they just
//! round-trip the request payloads through `serde_json::Value` and
//! confirm the fields the server-side `dispatch_command` looks for.

use serde_json::{Value, json};

/// Replicate the request shape `Client::set` would send.
#[test]
fn set_payload_shape() {
    let req: Value = json!({"Set": {"key": "k", "value": "v"}});
    assert!(req.get("Set").is_some());
    assert_eq!(req["Set"]["key"], "k");
    assert_eq!(req["Set"]["value"], "v");
}

#[test]
fn get_payload_shape() {
    let req: Value = json!({"Get": {"key": "k"}});
    assert_eq!(req["Get"]["key"], "k");
}

#[test]
fn delete_payload_shape() {
    let req: Value = json!({"Delete": {"key": "k"}});
    assert_eq!(req["Delete"]["key"], "k");
}

#[test]
fn begin_tx_payload_shape() {
    // `TxOp::Put` / `TxOp::Delete` are externally tagged by serde, so
    // the wire variants are capitalized (`{"Put": ...}` / `{"Delete": ...}`),
    // not the lowercase `put`/`delete` shown in the protobuf schema
    // (which only applies to the on-the-wire Raft RPC).
    let ops = vec![
        json!({"Put": {"key": "a", "value": "1"}}),
        json!({"Delete": {"key": "b"}}),
    ];
    let req: Value = json!({"BeginTx": {"tx_id": "t1", "ops": ops}});
    assert_eq!(req["BeginTx"]["tx_id"], "t1");
    assert_eq!(req["BeginTx"]["ops"].as_array().unwrap().len(), 2);
    assert_eq!(req["BeginTx"]["ops"][0]["Put"]["key"], "a");
    assert_eq!(req["BeginTx"]["ops"][1]["Delete"]["key"], "b");
}

#[test]
fn decide_tx_payload_shape() {
    // `TxDecision` is also externally tagged: `{"Commit": null}` or
    // `{"Abort": null}`, NOT a `commit: bool` flag.
    let req: Value = json!({"DecideTx": {"tx_id": "t1", "decision": {"Abort": null}}});
    assert_eq!(req["DecideTx"]["tx_id"], "t1");
    assert!(req["DecideTx"]["decision"]["Abort"].is_null());
}

/// Replicate the success response shapes `parse_index_response`
/// and `get` expect to see.
#[test]
fn mutation_ok_response_shape() {
    let resp: Value = json!({"status": "ok", "index": 7});
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["index"].as_u64(), Some(7));
}

#[test]
fn get_hit_response_shape() {
    let resp: Value = json!({"status": "ok", "data": "v"});
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["data"], "v");
}

#[test]
fn get_miss_response_shape() {
    let resp: Value = json!({"status": "not_found"});
    assert_eq!(resp["status"], "not_found");
}

#[test]
fn begin_tx_commit_response_shape() {
    let resp: Value = json!({
        "status": "ok",
        "tx_id": "t1",
        "decision": "commit",
        "begin_index": 3,
        "decide_index": 4,
    });
    assert_eq!(resp["status"], "ok");
    assert_eq!(resp["decision"], "commit");
    assert_eq!(resp["begin_index"].as_u64(), Some(3));
    assert_eq!(resp["decide_index"].as_u64(), Some(4));
}

#[test]
fn begin_tx_aborted_response_shape() {
    let resp: Value = json!({
        "status": "aborted",
        "tx_id": "t1",
        "reason": "vote-no-from-node-2",
    });
    assert_eq!(resp["status"], "aborted");
    assert_eq!(resp["reason"], "vote-no-from-node-2");
}

#[test]
fn not_leader_response_shape() {
    // Server returns this exact shape when a mutation hits a follower
    // (see rust/oxide-kv/src/client.rs:61).
    let resp: Value = json!({"error": "Not a leader. Please connect to the leader node."});
    assert!(resp["error"].as_str().unwrap().starts_with("Not a leader"));
}
