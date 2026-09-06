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
    spawn as spawn_writer, test_hooks, CommittedSyncUncertainError, FileMutate, FileRewrite,
    MutationIdentity, RequestIdReuseConflict, TxAppend, TxIdPolicy,
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
async fn single_transaction_sync_retry_preserves_committed_bytes_and_identity() {
    let _guard = hook_test_lock();
    for mode in 0..4 {
        test_hooks::reset();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("node.org");
        let tx_path = tmp.path().join("tx/2026-08.org");
        let retained_path = tmp.path().join("retained.org");
        std::fs::write(&target, "before\n").unwrap();
        let bus = EventBus::new();
        let mut events = bus.subscribe();
        let handle = spawn_writer(bus);
        let transforms = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let call = |payload: &'static str| {
            let handle = handle.clone();
            let target = target.clone();
            let tx = minted_tx_append(tx_path.clone(), "placeholder", "req-single-sync");
            let transforms = transforms.clone();
            async move {
                let mutation = MutationIdentity::new("task.created", "orgasmic", payload);
                let rewrites = vec![FileRewrite {
                    path: target.clone(),
                    new_contents: payload.as_bytes().to_vec(),
                }];
                let file = FileMutate {
                    path: target,
                    transform: Box::new(move |_| {
                        transforms.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(payload.as_bytes().to_vec())
                    }),
                };
                match mode {
                    0 => handle.transaction(rewrites, tx).await,
                    1 => handle
                        .transaction_mutation(rewrites, tx, mutation, "TASK-SYNC".into())
                        .await
                        .map(|r| r.tx_id),
                    2 => handle.transaction_mutate_file(file, tx, mutation).await,
                    _ => handle
                        .transaction_mutate_file_mutation(file, tx, mutation, "TASK-SYNC".into())
                        .await
                        .map(|r| {
                            assert_eq!(r.mutation_id, "TASK-SYNC");
                            r.tx_id
                        }),
                }
            }
        };
        test_hooks::fail_next_sync(1);
        let error = call("after\n").await.unwrap_err();
        assert!(
            error
                .downcast_ref::<CommittedSyncUncertainError>()
                .is_some(),
            "mode {mode}: {error}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
        assert_eq!(test_hooks::sync_count(), 0);
        assert!(handle.cached_tx_id("req-single-sync").await.is_none());
        let wrong_api = handle
            .append_tx(
                minted_tx_append(tx_path.clone(), "placeholder", "req-single-sync"),
                None,
            )
            .await
            .unwrap_err();
        assert!(wrong_api.downcast_ref::<RequestIdReuseConflict>().is_some());
        let bytes = std::fs::read(&tx_path).unwrap();
        let entries = parse_tx_file(std::str::from_utf8(&bytes).unwrap(), "tx").unwrap();
        assert_eq!(entries.len(), 1);
        let id = entries[0].tx_id.clone();
        let collision = call("different\n").await.unwrap_err();
        assert!(
            collision.downcast_ref::<RequestIdReuseConflict>().is_some(),
            "{collision}"
        );
        assert_eq!(
            test_hooks::sync_attempt_count(),
            1,
            "collision must not sync"
        );

        // A pathname replacement must not redirect the retained-descriptor retry.
        std::fs::rename(&tx_path, &retained_path).unwrap();
        std::fs::write(&tx_path, "foreign replacement\n").unwrap();
        test_hooks::fail_next_sync(1);
        let error = call("after\n").await.unwrap_err();
        assert!(
            error.to_string().contains("durability remains uncertain"),
            "{error}"
        );
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        if mode == 1 || mode == 3 {
            let recovered = handle
                .recover_mutation(
                    "req-single-sync",
                    &MutationIdentity::new("task.created", "orgasmic", "after\n"),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(recovered.tx_id, id);
            assert_eq!(recovered.mutation_id, "TASK-SYNC");
        } else {
            assert_eq!(call("after\n").await.unwrap(), id);
        }
        match events.try_recv().unwrap().payload {
            orgasmic_daemon::events::EventPayload::TxAppended {
                project_id,
                tx_id,
                ty,
            } => {
                assert_eq!(project_id.as_deref(), Some("orgasmic"));
                assert_eq!(tx_id, id);
                assert_eq!(
                    ty, "manager.action",
                    "publish the original tx type, not the caller identity label"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert_eq!(call("after\n").await.unwrap(), id);
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(test_hooks::sync_attempt_count(), 3);
        assert_eq!(test_hooks::sync_count(), 1);
        assert_eq!(std::fs::read(&retained_path).unwrap(), bytes);
        assert_eq!(
            std::fs::read_to_string(&tx_path).unwrap(),
            "foreign replacement\n"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after\n");
        assert_eq!(
            transforms.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(mode >= 2)
        );
    }
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
        .downcast_ref::<RequestIdReuseConflict>()
        .is_some());

    let tx_error = handle
        .transaction_multi(original_rewrite, make_txs("different"))
        .await
        .expect_err("tx semantic collision must fail closed");
    assert!(tx_error.downcast_ref::<RequestIdReuseConflict>().is_some());
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

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn task_create_sync_retry_projects_the_original_node() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let home = orgasmic_core::Home::at(tmp.path().join("home"));
    home.ensure().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".orgasmic")).unwrap();
    std::fs::write(
        project.join(".orgasmic/project.org"),
        "#+orgasmic_version: 1\n* PROJECT orgasmic\n:PROPERTIES:\n:ID: orgasmic\n:END:\n",
    )
    .unwrap();
    orgasmic_core::projects::register_project(&home, &project, "orgasmic", "main").unwrap();
    let running = orgasmic_daemon::Daemon::run(
        home.clone(),
        orgasmic_daemon::DaemonOptions {
            bind_override: Some("127.0.0.1".parse().unwrap()),
            port_override: Some(0),
            fs_watcher_enabled: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let token = std::fs::read_to_string(home.auth_token()).unwrap();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let url = format!("http://{}/api/projects/orgasmic/tasks", running.addr);
    let request = serde_json::json!({"title": "Sync recovery", "request_id": "create-sync-retry"});
    test_hooks::fail_next_sync(1);
    let first = client
        .post(&url)
        .bearer_auth(token.trim())
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let first: serde_json::Value = first.json().await.unwrap();
    assert_eq!(first["committed"], true);
    assert_eq!(first["durability"], "uncertain");
    let nodes: Vec<_> = std::fs::read_dir(project.join(".orgasmic/tasks"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(nodes.len(), 1);
    let id = nodes[0].file_name().unwrap().to_str().unwrap();
    let journal = nodes[0].join("journal.org");
    let bytes = std::fs::read(&journal).unwrap();
    let entries = orgasmic_core::node_kernel::parse_journal(
        std::str::from_utf8(&bytes).unwrap(),
        "journal.org",
    )
    .unwrap();
    assert_eq!(entries.len(), 1);
    let retry = client
        .post(&url)
        .bearer_auth(token.trim())
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = retry.status();
    let retry: serde_json::Value = retry.json().await.unwrap();
    assert!(status.is_success(), "{status}: {retry}");
    assert_eq!(retry["id"], id);
    assert_eq!(retry["tx_id"], entries[0].entry_id);
    let visible = client
        .get(format!("{url}/{id}"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert!(
        visible.status().is_success(),
        "retry must project the original generated node"
    );
    assert_eq!(
        std::fs::read_dir(project.join(".orgasmic/tasks"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
    for (collection, id) in [("decisions", "dec_A1B2C"), ("glossary", "term_A1B2C")] {
        let endpoint = format!("http://{}/api/{collection}", running.addr);
        let request = serde_json::json!({"project": "orgasmic", "id": id,
            "title": "Recovery example", "request_id": format!("recover-{collection}")});
        let call = |request: serde_json::Value| {
            client
                .post(&endpoint)
                .bearer_auth(token.trim())
                .json(&request)
                .send()
        };
        test_hooks::fail_next_sync(1);
        let first = call(request.clone()).await.unwrap();
        let status = first.status();
        let body: serde_json::Value = first.json().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "{collection}: {body}"
        );
        let node = project.join(".orgasmic").join(collection).join(id);
        let journal = std::fs::read(node.join("journal.org")).unwrap();
        let source = std::fs::read(node.join("node.org")).unwrap();
        test_hooks::fail_next_sync(1);
        let again = call(request.clone()).await.unwrap();
        let status = again.status();
        let body: serde_json::Value = again.json().await.unwrap();
        assert_eq!(
            status,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "pending recovery must precede the existing-node guard: {body}"
        );
        assert_eq!(body["durability"], "uncertain");
        let mut changed = request.clone();
        changed["title"] = "Different payload".into();
        assert_eq!(
            call(changed).await.unwrap().status(),
            reqwest::StatusCode::CONFLICT
        );
        let recovered = call(request.clone()).await.unwrap();
        let status = recovered.status();
        let body: serde_json::Value = recovered.json().await.unwrap();
        assert!(status.is_success(), "{collection}: {body}");
        assert_eq!(body["id"], id);
        let repeated: serde_json::Value = call(request).await.unwrap().json().await.unwrap();
        assert_eq!(repeated["tx_id"], body["tx_id"]);
        assert_eq!(std::fs::read(node.join("journal.org")).unwrap(), journal);
        assert_eq!(std::fs::read(node.join("node.org")).unwrap(), source);
    }
    running.shutdown.send(()).unwrap();
    running.join.await.unwrap();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn sync_retry_refuses_a_reopened_unrelated_ledger() {
    let _guard = hook_test_lock();
    test_hooks::reset();
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("node.org");
    let tx_path = tmp.path().join("2026-08.org");
    let retained = tmp.path().join("retained.org");
    let handle = spawn_writer(EventBus::new());
    let rewrites = vec![FileRewrite {
        path: target.clone(),
        new_contents: b"committed\n".to_vec(),
    }];
    let tx = minted_tx_append(tx_path.clone(), "original", "original-request");
    test_hooks::fail_next_sync(1);
    assert!(handle
        .transaction(rewrites.clone(), tx.clone())
        .await
        .is_err());
    let original = std::fs::read(&tx_path).unwrap();
    std::fs::rename(&tx_path, &retained).unwrap();
    handle
        .append_tx(
            minted_tx_append(tx_path.clone(), "foreign", "foreign-request"),
            None,
        )
        .await
        .unwrap();
    let replacement = std::fs::read(&tx_path).unwrap();
    let syncs = test_hooks::sync_count();
    let error = handle.transaction(rewrites, tx).await.unwrap_err();
    assert!(error
        .downcast_ref::<CommittedSyncUncertainError>()
        .is_some());
    assert!(
        error.to_string().contains("no longer contains transaction"),
        "{error}"
    );
    assert_eq!(
        test_hooks::sync_count(),
        syncs,
        "must not sync an unrelated ledger as proof"
    );
    assert_eq!(std::fs::read(&retained).unwrap(), original);
    assert_eq!(std::fs::read(&tx_path).unwrap(), replacement);
    assert_eq!(std::fs::read_to_string(target).unwrap(), "committed\n");
}
