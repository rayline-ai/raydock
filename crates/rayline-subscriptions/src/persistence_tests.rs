use super::*;
use crate::routing_state::RoutingBlockKind;
use std::os::unix::fs::{PermissionsExt, symlink};

fn root() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().expect("temporary directory");
    // Resolve the OS-provided macOS /var alias, not user-supplied state links.
    let path = temporary
        .path()
        .canonicalize()
        .expect("physical temp path")
        .join("routing-state");
    (temporary, path)
}

fn block(kind: RoutingBlockKind) -> RoutingBlock {
    RoutingBlock {
        kind,
        scope: ClaimScope::Global,
        claim_key: Some("five_hour".to_owned()),
        rejected_sequence: 2,
        credential_generation: 1,
        observed_at_ms: 10,
        retry_at_ms: 100,
        resets_at_ms: Some(100),
    }
}

fn records() -> Vec<AccountBlocks> {
    vec![AccountBlocks {
        account_id: "account-private".to_owned(),
        source_identity: "/private/source-path".to_owned(),
        blocks: vec![block(RoutingBlockKind::Quota)],
    }]
}

fn configured() -> Vec<StoreAccount> {
    vec![StoreAccount {
        account_id: "account-private".to_owned(),
        source_identity: "/private/source-path".to_owned(),
    }]
}

#[test]
fn restart_restores_bound_restrictions_not_raw_identities() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory.clone(), "pool-private").expect("store");
    store.save(&records()).expect("save");
    drop(store);
    let restored = BlockStore::new(directory.clone(), "pool-private")
        .expect("reopened")
        .load(&configured(), 50)
        .expect("load");
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].account_id, "account-private");
    assert_eq!(restored[0].blocks, records()[0].blocks);
    let json = fs::read_to_string(directory.join(SNAPSHOT)).expect("snapshot");
    assert!(!json.contains("account-private"));
    assert!(!json.contains("pool-private"));
    assert!(!json.contains("/private/source-path"));
}

#[test]
fn removed_accounts_changed_sources_and_changed_config_do_not_restore() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory.clone(), "config-v1").expect("store");
    store.save(&records()).expect("save");
    assert!(store.load(&[], 0).expect("removed").is_empty());
    let mut current = configured();
    current[0].source_identity = "changed-source".to_owned();
    assert!(store.load(&current, 0).expect("changed source").is_empty());
    current = configured();
    current[0].account_id = "new-account".to_owned();
    assert!(store.load(&current, 0).expect("changed account").is_empty());
    let changed = BlockStore::new(directory, "config-v2").expect("store");
    assert!(matches!(
        changed.load(&configured(), 0),
        Err(StoreError::Incompatible)
    ));
}

#[test]
fn expired_restrictions_remain_for_state_controlled_half_open_revalidation() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory, "pool").expect("store");
    let mut records = records();
    records[0].blocks.extend([
        block(RoutingBlockKind::Overage),
        block(RoutingBlockKind::Authentication),
        block(RoutingBlockKind::Entitlement),
    ]);
    store.save(&records).expect("save");
    assert_eq!(
        store.load(&configured(), 99).expect("before expiry")[0]
            .blocks
            .len(),
        4
    );
    let restored = store.load(&configured(), 100).expect("expired");
    assert_eq!(restored[0].blocks, records[0].blocks);
}

#[test]
fn absent_corrupt_and_incompatible_state_have_distinct_outcomes() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    assert!(store.load(&configured(), 0).expect("absent").is_empty());
    store.save(&records()).expect("save");
    fs::write(directory.join(SNAPSHOT), b"{secret-parser-excerpt").expect("corrupt");
    let error = store.load(&configured(), 0).expect_err("corruption");
    assert!(matches!(error, StoreError::Corrupt));
    assert!(!error.to_string().contains("secret-parser-excerpt"));
    store.save(&records()).expect("replace corrupt file safely");
    let mut json: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join(SNAPSHOT)).expect("read")).expect("json");
    json["schema"] = serde_json::json!(999);
    fs::write(
        directory.join(SNAPSHOT),
        serde_json::to_vec(&json).expect("json"),
    )
    .expect("write schema");
    assert!(matches!(
        store.load(&configured(), 0),
        Err(StoreError::Incompatible)
    ));
}

#[test]
fn snapshot_file_and_directory_are_private_and_unsafe_permissions_rejected() {
    let (_temporary, directory) = root();
    let path = directory.join("new").join("store");
    let store = BlockStore::new(path.clone(), "pool").expect("nested store");
    store.save(&records()).expect("save");
    assert_eq!(
        fs::metadata(&path).expect("directory").permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(path.join(SNAPSHOT))
            .expect("file")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::set_permissions(path.join(SNAPSHOT), fs::Permissions::from_mode(0o644)).expect("chmod");
    assert!(matches!(
        store.load(&configured(), 0),
        Err(StoreError::UnsafeFilesystem)
    ));
    assert!(matches!(
        store.save(&records()),
        Err(StoreError::UnsafeFilesystem)
    ));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod directory");
    assert!(matches!(
        BlockStore::new(path, "pool"),
        Err(StoreError::UnsafeFilesystem)
    ));
}

#[test]
fn symlink_directories_ancestors_and_snapshot_files_are_rejected() {
    let (_temporary, directory) = root();
    let actual = directory.join("actual");
    let store = BlockStore::new(actual.clone(), "pool").expect("store");
    let link = directory.join("linked");
    symlink(&actual, &link).expect("directory link");
    assert!(BlockStore::new(link.clone(), "pool").is_err());
    assert!(BlockStore::new(link.join("child"), "pool").is_err());
    let outside = directory.join("outside");
    fs::write(&outside, b"untouched").expect("outside");
    symlink(&outside, actual.join(SNAPSHOT)).expect("snapshot link");
    assert!(matches!(
        store.save(&records()),
        Err(StoreError::UnsafeFilesystem)
    ));
    assert!(matches!(
        store.load(&configured(), 0),
        Err(StoreError::UnsafeFilesystem)
    ));
    assert_eq!(fs::read(&outside).expect("outside"), b"untouched");
}

#[test]
fn hardlinked_snapshot_and_nonregular_files_are_rejected_without_blocking() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    store.save(&records()).expect("save");
    fs::hard_link(directory.join(SNAPSHOT), directory.join("alias")).expect("hard link");
    assert!(matches!(
        store.load(&configured(), 0),
        Err(StoreError::UnsafeFilesystem)
    ));
    assert!(matches!(
        store.save(&records()),
        Err(StoreError::UnsafeFilesystem)
    ));
    fs::remove_file(directory.join(SNAPSHOT)).expect("remove file");
    fs::create_dir(directory.join(SNAPSHOT)).expect("nonregular");
    assert!(matches!(
        store.load(&configured(), 0),
        Err(StoreError::UnsafeFilesystem)
    ));
}

#[test]
fn oversize_input_and_output_fail_without_replacing_previous_snapshot() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    store.save(&records()).expect("save");
    let before = fs::read(directory.join(SNAPSHOT)).expect("snapshot");
    let mut too_many = records();
    too_many[0].blocks = vec![block(RoutingBlockKind::Quota); MAX_BLOCKS_PER_ACCOUNT + 1];
    assert!(matches!(store.save(&too_many), Err(StoreError::TooLarge)));
    assert_eq!(
        fs::read(directory.join(SNAPSHOT)).expect("unchanged"),
        before
    );
    let big: Vec<_> = (0..MAX_ACCOUNTS)
        .map(|index| AccountBlocks {
            account_id: index.to_string(),
            source_identity: "source".to_owned(),
            blocks: vec![block(RoutingBlockKind::Quota); MAX_BLOCKS_PER_ACCOUNT],
        })
        .collect();
    assert!(matches!(store.save(&big), Err(StoreError::TooLarge)));
    assert_eq!(
        fs::read(directory.join(SNAPSHOT)).expect("unchanged"),
        before
    );
    fs::write(directory.join(SNAPSHOT), vec![b' '; MAX_SNAPSHOT_BYTES + 1]).expect("oversize");
    assert!(matches!(
        store.load(&configured(), 0),
        Err(StoreError::TooLarge)
    ));
}

#[test]
fn free_text_in_blocks_is_never_written_and_recovery_atomically_replaces_state() {
    let (_temporary, directory) = root();
    let store = BlockStore::new(directory.clone(), "pool").expect("store");
    store.save(&records()).expect("save");
    let mut unsafe_records = records();
    unsafe_records[0].blocks[0].claim_key = Some("secret-free-text".to_owned());
    assert!(matches!(
        store.save(&unsafe_records),
        Err(StoreError::Corrupt)
    ));
    unsafe_records[0].blocks[0].claim_key = None;
    unsafe_records[0].blocks[0].scope = ClaimScope::Surface("private-prompt".to_owned());
    assert!(matches!(
        store.save(&unsafe_records),
        Err(StoreError::Corrupt)
    ));
    let mut recovered = records();
    recovered[0].blocks.clear();
    store.save(&recovered).expect("clear recovered restriction");
    drop(store);
    let store = BlockStore::new(directory.clone(), "pool").expect("reopen");
    assert!(
        store.load(&configured(), 0).expect("recovery")[0]
            .blocks
            .is_empty()
    );
    assert_eq!(fs::read_dir(directory).expect("entries").count(), 1);
}

#[test]
fn pinned_directory_cannot_be_redirected_after_open() {
    let (_temporary, directory) = root();
    let original = directory.join("original");
    let store = BlockStore::new(original.clone(), "pool").expect("store");
    let moved = directory.join("moved");
    fs::rename(&original, &moved).expect("move");
    let outside = directory.join("outside");
    fs::create_dir(&outside).expect("outside");
    symlink(&outside, &original).expect("redirect");
    store.save(&records()).expect("pinned save");
    assert!(moved.join(SNAPSHOT).exists());
    assert!(!outside.join(SNAPSHOT).exists());
    assert_eq!(
        store.load(&configured(), 0).expect("pinned read")[0]
            .blocks
            .len(),
        1
    );
}
