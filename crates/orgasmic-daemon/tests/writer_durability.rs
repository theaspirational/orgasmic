//! TASK-149: tx append fsync-before-ack and group commit.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

static HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

fn hook_test_lock() -> std::sync::MutexGuard<'static, ()> {
    HOOK_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

use orgasmic_core::tx::{parse_tx_file, TxEntry};
use orgasmic_daemon::events::EventBus;
use orgasmic_daemon::writer::{
    spawn as spawn_writer, test_hooks, FileRewrite, TxAppend, TxIdPolicy,
};
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

fn minted_tx_append(tx_path: PathBuf, placeholder: &str, request_id: &str) -> TxAppend {
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
async fn rewrite_transaction_rolls_back_when_tx_sync_fails() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("tasks.org");
    let tx_path = tmp.path().join("tx").join("2026-08.org");
    std::fs::write(&target, "before\n").unwrap();
    let handle = spawn_writer(EventBus::new());

    test_hooks::fail_next_sync(1);
    let err = handle
        .transaction(
            vec![FileRewrite {
                path: target.clone(),
                new_contents: b"after\n".to_vec(),
            }],
            TxAppend {
                tx_path,
                entry: sample_entry("tx-rewrite-sync-fail"),
                project_id: Some("orgasmic".into()),
                tx_id_policy: TxIdPolicy::Preserve,
                request_id: Some("req-rewrite-sync-fail".into()),
            },
        )
        .await
        .expect_err("injected tx sync failure must reject the transaction");

    assert!(err.to_string().contains("fsync"), "unexpected error: {err}");
    assert_eq!(std::fs::read_to_string(target).unwrap(), "before\n");
    assert_eq!(test_hooks::sync_attempt_count(), 1);
    assert_eq!(test_hooks::sync_count(), 0);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn multi_transaction_orders_entries_under_one_flock_and_one_sync() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("tasks.org");
    let tx_path = tmp.path().join("tx").join("2026-08.org");
    std::fs::write(&target, "before\n").unwrap();
    let handle = spawn_writer(EventBus::new());

    let results = handle
        .transaction_multi(
            vec![FileRewrite {
                path: target.clone(),
                new_contents: b"after\n".to_vec(),
            }],
            vec![
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-close"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: Some("req-close".into()),
                },
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-transition"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: Some("req-transition".into()),
                },
            ],
        )
        .await
        .expect("multi transaction");

    assert_eq!(
        results
            .iter()
            .map(|result| result.tx_id.as_str())
            .collect::<Vec<_>>(),
        ["tx-close", "tx-transition"]
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "after\n");
    let entries = parse_tx_file(&std::fs::read_to_string(tx_path).unwrap(), "tx").unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.tx_id.as_str())
            .collect::<Vec<_>>(),
        ["tx-close", "tx-transition"]
    );
    assert_eq!(test_hooks::flock_count(), 1);
    assert_eq!(test_hooks::sync_count(), 1);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn injected_multi_commit_failure_lands_neither_leg() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("tasks.org");
    let tx_path = tmp.path().join("tx").join("2026-08.org");
    std::fs::write(&target, "before\n").unwrap();
    let handle = spawn_writer(EventBus::new());
    test_hooks::fail_next_multi_before_commit(1);

    let error = handle
        .transaction_multi(
            vec![FileRewrite {
                path: target.clone(),
                new_contents: b"after\n".to_vec(),
            }],
            vec![
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-close"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: Some("req-close-fail".into()),
                },
                TxAppend {
                    tx_path: tx_path.clone(),
                    entry: sample_entry("tx-transition"),
                    project_id: Some("orgasmic".into()),
                    tx_id_policy: TxIdPolicy::Preserve,
                    request_id: Some("req-transition-fail".into()),
                },
            ],
        )
        .await
        .expect_err("injected boundary must fail before the commit");
    assert!(error.to_string().contains("injected failure before multi"));
    assert_eq!(std::fs::read_to_string(target).unwrap(), "before\n");
    assert!(!tx_path.exists(), "neither ledger leg may land");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn post_append_sync_failure_keeps_rewrites_and_retry_syncs_without_duplicate_pair() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("tasks.org");
    let tx_path = tmp.path().join("tx").join("2026-08.org");
    std::fs::write(&target, "before\n").unwrap();
    let handle = spawn_writer(EventBus::new());
    let rewrites = vec![FileRewrite {
        path: target.clone(),
        new_contents: b"after\n".to_vec(),
    }];
    let txs = vec![
        TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-close-sync-uncertain"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: Some("req-close-sync-uncertain".into()),
        },
        TxAppend {
            tx_path: tx_path.clone(),
            entry: sample_entry("tx-transition-sync-uncertain"),
            project_id: Some("orgasmic".into()),
            tx_id_policy: TxIdPolicy::Preserve,
            request_id: Some("req-transition-sync-uncertain".into()),
        },
    ];

    test_hooks::fail_next_sync(1);
    let error = handle
        .transaction_multi(rewrites.clone(), txs.clone())
        .await
        .expect_err("failed durability acknowledgement must be explicit");
    assert!(
        error
            .to_string()
            .contains("committed but durability is uncertain"),
        "unexpected error: {error}"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
    let entries = parse_tx_file(&std::fs::read_to_string(&tx_path).unwrap(), "tx").unwrap();
    assert_eq!(
        entries.len(),
        2,
        "both tx legs stay convergent with rewrites"
    );

    test_hooks::fail_next_sync(1);
    let still_uncertain = handle
        .transaction_multi(rewrites.clone(), txs.clone())
        .await
        .expect_err("a retained descriptor sync failure must remain explicit");
    assert!(
        still_uncertain
            .to_string()
            .contains("durability remains uncertain"),
        "unexpected retained-descriptor error: {still_uncertain}"
    );
    let entries = parse_tx_file(&std::fs::read_to_string(&tx_path).unwrap(), "tx").unwrap();
    assert_eq!(entries.len(), 2, "failed re-sync must not append a pair");

    let retried = handle
        .transaction_multi(rewrites, txs)
        .await
        .expect("same semantic retry must sync, not append again");
    assert_eq!(
        retried
            .iter()
            .map(|result| result.tx_id.as_str())
            .collect::<Vec<_>>(),
        ["tx-close-sync-uncertain", "tx-transition-sync-uncertain"]
    );
    let entries = parse_tx_file(&std::fs::read_to_string(&tx_path).unwrap(), "tx").unwrap();
    assert_eq!(entries.len(), 2, "retry must not append a duplicate pair");
    assert_eq!(test_hooks::sync_attempt_count(), 3);
    assert_eq!(test_hooks::sync_count(), 1);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn multi_transaction_request_id_collisions_fail_closed_on_semantic_changes() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("tasks.org");
    let tx_path = tmp.path().join("tx").join("2026-08.org");
    std::fs::write(&target, "before\n").unwrap();
    let handle = spawn_writer(EventBus::new());
    let make_txs = |transition_reason: &str| {
        let mut transition = sample_entry("tx-collision-transition");
        transition.reason = Some(transition_reason.to_string());
        vec![
            TxAppend {
                tx_path: tx_path.clone(),
                entry: sample_entry("tx-collision-close"),
                project_id: Some("orgasmic".into()),
                tx_id_policy: TxIdPolicy::Preserve,
                request_id: Some("req-collision-close".into()),
            },
            TxAppend {
                tx_path: tx_path.clone(),
                entry: transition,
                project_id: Some("orgasmic".into()),
                tx_id_policy: TxIdPolicy::Preserve,
                request_id: Some("req-collision-transition".into()),
            },
        ]
    };
    let original_rewrite = vec![FileRewrite {
        path: target.clone(),
        new_contents: b"after\n".to_vec(),
    }];
    handle
        .transaction_multi(original_rewrite.clone(), make_txs("original"))
        .await
        .unwrap();

    let rewrite_error = handle
        .transaction_multi(
            vec![FileRewrite {
                path: target,
                new_contents: b"different\n".to_vec(),
            }],
            make_txs("original"),
        )
        .await
        .expect_err("rewrite collision must fail closed");
    assert!(rewrite_error
        .to_string()
        .contains("different multi-transaction"));

    let tx_error = handle
        .transaction_multi(original_rewrite, make_txs("different"))
        .await
        .expect_err("tx semantic collision must fail closed");
    assert!(tx_error.to_string().contains("different multi-transaction"));
    let entries = parse_tx_file(&std::fs::read_to_string(&tx_path).unwrap(), "tx").unwrap();
    assert_eq!(entries.len(), 2, "collisions must not append anything");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn concurrent_multi_transactions_never_interleave_their_entries() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let tx_path = tmp.path().join("tx").join("2026-08.org");
    let handle = Arc::new(spawn_writer(EventBus::new()));
    let mut tasks = JoinSet::new();
    for group in ["a", "b"] {
        let handle = Arc::clone(&handle);
        let tx_path = tx_path.clone();
        tasks.spawn(async move {
            handle
                .transaction_multi(
                    Vec::new(),
                    [1, 2]
                        .into_iter()
                        .map(|index| TxAppend {
                            tx_path: tx_path.clone(),
                            entry: sample_entry(&format!("tx-{group}-{index}")),
                            project_id: Some("orgasmic".into()),
                            tx_id_policy: TxIdPolicy::Preserve,
                            request_id: Some(format!("req-{group}-{index}")),
                        })
                        .collect(),
                )
                .await
                .unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    let ids = parse_tx_file(&std::fs::read_to_string(tx_path).unwrap(), "tx")
        .unwrap()
        .into_iter()
        .map(|entry| entry.tx_id)
        .collect::<Vec<_>>();
    assert!(
        ids == ["tx-a-1", "tx-a-2", "tx-b-1", "tx-b-2"]
            || ids == ["tx-b-1", "tx-b-2", "tx-a-1", "tx-a-2"],
        "concurrent groups interleaved: {ids:?}"
    );
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
async fn tx_append_reopens_after_path_inode_swap() {
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
            minted_tx_append(tx_path.clone(), "first", "req-swap-1"),
            None,
        )
        .await
        .expect("first append");

    let replacement = tx_dir.join("replacement.org");
    std::fs::write(
        &replacement,
        "#+title: replacement tx\n#+orgasmic_version: 1\n\n* TX 2026-06-12 12:00 manager.action orgasmic\n:PROPERTIES:\n:TX_ID:        tx-20260612-orgasmic-0040\n:TIME:         [2026-06-12 Fri 12:00:00]\n:TYPE:         manager.action\n:ACTOR:        dev@example.com\n:MACHINE:      host.local\n:PROJECT:      orgasmic\n:END:\n",
    )
    .unwrap();
    std::fs::rename(&replacement, &tx_path).unwrap();

    let second = handle
        .append_tx(
            minted_tx_append(tx_path.clone(), "second", "req-swap-2"),
            None,
        )
        .await
        .expect("append after inode swap");

    let source = std::fs::read_to_string(&tx_path).unwrap();
    assert!(source.contains(":TX_ID:        tx-20260612-orgasmic-0040"));
    assert!(source.contains(&format!(":TX_ID:        {}", second.tx_id)));
    assert!(
        !source.contains(&format!(":TX_ID:        {}", first.tx_id)),
        "post-swap append must land in the replacement file at the path, not the orphaned inode"
    );
}
