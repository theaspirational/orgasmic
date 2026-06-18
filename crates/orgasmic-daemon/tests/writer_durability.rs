//! TASK-149: tx append fsync-before-ack, group commit, cached project-tx sequence.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

static HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

fn hook_test_lock() -> std::sync::MutexGuard<'static, ()> {
    HOOK_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

use orgasmic_core::tx::TxEntry;
use orgasmic_daemon::events::EventBus;
use orgasmic_daemon::writer::{spawn as spawn_writer, test_hooks, TxAppend, TxIdPolicy};
use tokio::task::JoinSet;

fn sample_entry(tx_id: &str) -> TxEntry {
    let mut e = TxEntry::new(
        tx_id,
        "manager.action",
        "[2026-06-12 Fri 12:00:00]",
        "dev@example.com",
        "host.local",
    );
    e.project = Some("orgasmic".into());
    e.reason = Some("test".into());
    e
}

fn project_seq_append(tx_path: PathBuf, placeholder: &str, request_id: &str) -> TxAppend {
    TxAppend {
        tx_path,
        entry: sample_entry(placeholder),
        project_id: Some("orgasmic".into()),
        tx_id_policy: TxIdPolicy::ProjectSequence {
            project_id: "orgasmic".into(),
            date: "20260612".into(),
        },
        request_id: Some(request_id.into()),
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn tx_append_acks_only_after_fsync() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let tx_path = tmp.path().join("tx").join("2026-06.org");
    let handle = spawn_writer(EventBus::new());

    test_hooks::fail_next_sync(1);
    let err = handle
        .append_tx(
            TxAppend {
                tx_path: tx_path.clone(),
                entry: sample_entry("tx-fsync-fail"),
                project_id: Some("orgasmic".into()),
                tx_id_policy: TxIdPolicy::Preserve,
                request_id: None,
            },
            Some("req-fsync-fail".into()),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("fsync"),
        "expected fsync failure, got {err}"
    );
    assert_eq!(
        test_hooks::sync_attempt_count(),
        1,
        "fsync must be attempted before ack"
    );
    assert_eq!(
        test_hooks::sync_count(),
        0,
        "failed fsync must not count as durable"
    );

    test_hooks::reset();
    handle
        .append_tx(
            TxAppend {
                tx_path: tx_path.clone(),
                entry: sample_entry("tx-fsync-ok"),
                project_id: Some("orgasmic".into()),
                tx_id_policy: TxIdPolicy::Preserve,
                request_id: None,
            },
            Some("req-fsync-ok".into()),
        )
        .await
        .expect("append after fsync recovery");
    assert_eq!(test_hooks::sync_count(), 1);
    let source = std::fs::read_to_string(&tx_path).unwrap();
    assert!(source.contains("tx-fsync-ok"));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn concurrent_tx_appends_group_commit_single_fsync() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let tx_path = tmp.path().join("tx").join("2026-06.org");
    let handle = Arc::new(spawn_writer(EventBus::new()));
    let n = 8_usize;

    let mut tasks = JoinSet::new();
    for i in 0..n {
        let handle = Arc::clone(&handle);
        let tx_path = tx_path.clone();
        tasks.spawn(async move {
            handle
                .append_tx(
                    TxAppend {
                        tx_path,
                        entry: sample_entry(&format!("tx-batch-{i}")),
                        project_id: Some("orgasmic".into()),
                        tx_id_policy: TxIdPolicy::Preserve,
                        request_id: None,
                    },
                    Some(format!("req-batch-{i}")),
                )
                .await
                .expect("batch append");
        });
    }
    while tasks.join_next().await.is_some() {}

    let syncs = test_hooks::sync_count();
    assert!(
        syncs < n as u64,
        "expected group commit: {syncs} syncs for {n} appends"
    );
    assert!(syncs >= 1, "expected at least one fsync");
    let source = std::fs::read_to_string(&tx_path).unwrap();
    for i in 0..n {
        assert!(
            source.contains(&format!("tx-batch-{i}")),
            "missing tx-batch-{i}"
        );
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn project_tx_sequence_cache_avoids_rescan_on_hot_path() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let tx_dir = tmp.path().join("tx");
    std::fs::create_dir_all(&tx_dir).unwrap();

    for month in 1..=12 {
        let path = tx_dir.join(format!("2025-{month:02}.org"));
        let body = format!(
            "#+title: orgasmic project tx 2025-{month:02}\n#+orgasmic_version: 1\n\n* TX 2025-{month:02}-01 10:00 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-2025{month:02}01-orgasmic-{month:04}\n:TIME:         [2025-{month:02}-01 Sat 10:00:00]\n:TYPE:         manager.action\n:ACTOR:        dev@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n"
        );
        std::fs::write(&path, body).unwrap();
    }

    let tx_path = tx_dir.join("2026-06.org");
    let handle = spawn_writer(EventBus::new());

    let first = handle
        .append_tx(
            project_seq_append(tx_path.clone(), "first", "req-seq-1"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(test_hooks::scan_count(), 1, "first append should scan once");

    let second = handle
        .append_tx(
            project_seq_append(tx_path.clone(), "second", "req-seq-2"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        test_hooks::scan_count(),
        1,
        "hot-path append must not re-scan tx directory"
    );
    assert_eq!(first.tx_id, "tx-20260612-orgasmic-0013");
    assert_eq!(second.tx_id, "tx-20260612-orgasmic-0014");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn tx_append_reopens_after_path_inode_swap_and_rescans_sequence() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let tx_dir = tmp.path().join("tx");
    std::fs::create_dir_all(&tx_dir).unwrap();
    std::fs::write(
        tx_dir.join("2026-05.org"),
        "#+title: orgasmic project tx 2026-05\n#+orgasmic_version: 1\n\n* TX 2026-05-01 10:00 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-20260501-orgasmic-0012\n:TIME:         [2026-05-01 Fri 10:00:00]\n:TYPE:         manager.action\n:ACTOR:        dev@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n",
    )
    .unwrap();

    let tx_path = tx_dir.join("2026-06.org");
    let handle = spawn_writer(EventBus::new());
    let first = handle
        .append_tx(
            project_seq_append(tx_path.clone(), "first", "req-swap-1"),
            None,
        )
        .await
        .expect("first append");
    assert_eq!(first.tx_id, "tx-20260612-orgasmic-0013");

    let replacement = tx_dir.join("replacement.org");
    std::fs::write(
        &replacement,
        "#+title: replacement tx\n#+orgasmic_version: 1\n\n* TX 2026-06-12 12:00 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-20260612-orgasmic-0040\n:TIME:         [2026-06-12 Fri 12:00:00]\n:TYPE:         manager.action\n:ACTOR:        dev@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n",
    )
    .unwrap();
    std::fs::rename(&replacement, &tx_path).unwrap();

    let second = handle
        .append_tx(
            project_seq_append(tx_path.clone(), "second", "req-swap-2"),
            None,
        )
        .await
        .expect("append after inode swap");
    assert_eq!(second.tx_id, "tx-20260612-orgasmic-0041");

    let source = std::fs::read_to_string(&tx_path).unwrap();
    assert!(source.contains(":TX_ID:        tx-20260612-orgasmic-0040"));
    assert!(source.contains(":TX_ID:        tx-20260612-orgasmic-0041"));
    assert!(
        !source.contains(":TX_ID:        tx-20260612-orgasmic-0013"),
        "post-swap append must land in the replacement file at the path, not the orphaned inode"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn corrupt_sibling_tx_file_does_not_block_appends() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let tx_dir = tmp.path().join("tx");
    std::fs::create_dir_all(&tx_dir).unwrap();
    std::fs::write(
        tx_dir.join("2026-05.org"),
        "#+title: orgasmic project tx 2026-05\n#+orgasmic_version: 1\n\n* TX 2026-05-21 22:10 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-20260521-orgasmic-0036\n:TIME:         [2026-05-21 Thu 22:10:00]\n:TYPE:         manager.action\n:ACTOR:        dev@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n",
    )
    .unwrap();
    std::fs::write(
        tx_dir.join("2026-04.org"),
        "this is not a valid org tx file\n* broken heading with body\nnot a drawer only\n",
    )
    .unwrap();

    let tx_path = tx_dir.join("2026-06.org");
    let handle = spawn_writer(EventBus::new());
    let res = handle
        .append_tx(
            project_seq_append(tx_path.clone(), "placeholder", "req-corrupt-sibling"),
            None,
        )
        .await
        .expect("append must succeed despite corrupt sibling");
    assert_eq!(res.tx_id, "tx-20260612-orgasmic-0037");
    let source = std::fs::read_to_string(&tx_path).unwrap();
    assert!(source.contains(":TX_ID:        tx-20260612-orgasmic-0037"));
}
