use super::*;
use crate::model::ModelFamily;
use crate::routing_state::RoutingBlockKind;
use std::os::unix::ffi::OsStrExt;

#[test]
fn arbitrary_model_names_and_duplicate_records_do_not_enter_snapshot() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let directory = temporary
        .path()
        .canonicalize()
        .expect("physical path")
        .join("routing-state");
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    let mut record = AccountBlocks {
        account_id: "a".to_owned(),
        source_identity: "source".to_owned(),
        blocks: vec![RoutingBlock {
            kind: RoutingBlockKind::Quota,
            scope: ClaimScope::Model(ModelFamily::from_requested_model("private-request-text")),
            claim_key: None,
            rejected_sequence: 1,
            credential_generation: 1,
            observed_at_ms: 1,
            retry_at_ms: 100,
            resets_at_ms: None,
        }],
    };
    assert!(matches!(
        store.save(&[record.clone()]),
        Err(StoreError::Corrupt)
    ));
    assert!(!directory.join(SNAPSHOT).exists());
    record.blocks[0].scope =
        ClaimScope::Model(ModelFamily::from_requested_model("claude-sonnet-4-5"));
    store.save(&[record.clone()]).expect("safe model");
    let digest = format!("sha256_{:x}", Sha256::digest(b"unknown-model"));
    record.blocks[0].scope = ClaimScope::Model(ModelFamily::from_requested_model(&digest));
    store.save(&[record.clone()]).expect("hashed model");
    let restored = store
        .load(
            &[StoreAccount {
                account_id: "a".to_owned(),
                source_identity: "source".to_owned(),
            }],
            0,
        )
        .expect("hashed restore");
    assert_eq!(restored[0].blocks, record.blocks);
    let raw = fs::read_to_string(directory.join(SNAPSHOT)).expect("snapshot");
    assert!(!raw.contains("unknown-model"));
    assert!(matches!(
        store.save(&[record.clone(), record]),
        Err(StoreError::Corrupt)
    ));
}

#[test]
fn fifo_snapshot_is_rejected_instead_of_waiting_for_a_writer() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let directory = temporary
        .path()
        .canonicalize()
        .expect("physical path")
        .join("routing-state");
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    let path =
        std::ffi::CString::new(directory.join(SNAPSHOT).as_os_str().as_bytes()).expect("path");
    // SAFETY: valid NUL-terminated temp path; no memory pointers are retained.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        store.load(&[], 0),
        Err(StoreError::UnsafeFilesystem)
    ));
    assert!(matches!(store.save(&[]), Err(StoreError::UnsafeFilesystem)));
}

#[test]
fn duplicate_and_unknown_json_fields_are_corrupt_not_silent_restores() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let directory = temporary
        .path()
        .canonicalize()
        .expect("physical path")
        .join("routing-state");
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    store.save(&[]).expect("initial snapshot");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join(SNAPSHOT)).expect("read")).expect("json");
    json["unexpected"] = serde_json::json!(true);
    fs::write(
        directory.join(SNAPSHOT),
        serde_json::to_vec(&json).expect("serialize"),
    )
    .expect("write");
    assert!(matches!(store.load(&[], 0), Err(StoreError::Corrupt)));
    let duplicate = format!(
        "{{\"schema\":1,\"schema\":1,\"pool\":\"{}\",\"accounts\":[]}}",
        store.pool
    );
    fs::write(directory.join(SNAPSHOT), duplicate).expect("duplicate schema");
    assert!(matches!(store.load(&[], 0), Err(StoreError::Corrupt)));
}
