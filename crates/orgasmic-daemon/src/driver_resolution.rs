// orgasmic:task_3NJ9K
//! The daemon's single driver-resolution seam.
//!
//! Every daemon code path that turns a `(mode, harness)` address — or a legacy
//! transport id — into a live [`WorkerDriver`] goes through this module. In a
//! normal build the functions here are a pass-through to the driver registry
//! and compile to exactly the call they replaced.
//!
//! Under `cfg(test)` they are a fence, and the fence is **default-deny**:
//! every mode resolves to a panic naming the address and pointing at
//! [`stub_driver`], except the in-process stub and the two mux modes named in
//! [`LIVE_TOOLING_MODES`]. A mode added to the registry tomorrow is refused by
//! this file without anyone editing it.
//!
//! The fence exists because a daemon unit test once built a
//! `StageWorker { driver: "acp-stdio", harness: "hermes" }` and guarded the
//! spawn with the comment *"unreachable without a hermes binary"* — true on CI,
//! false on the developer machine where the test then spawned real, billed
//! agents that outlived it, became PPID-1 orphans, and mutated `main`
//! (TASK-3NJ9K, the class behind TASK-95SGV.2). The mechanism was mechanical,
//! not exotic: `acp-stdio` upgrades a simulated acquire to a real subprocess
//! whenever `command_available(<harness binary>)` says the harness is on
//! `$PATH` (`modes/acp_stdio.rs`), so the same test was inert on one host and a
//! provider spawn on another.
//!
//! Three properties matter and are all load-bearing:
//!
//! - The refusal never consults the host. It is a pure function of the address,
//!   so it fires identically with and without a provider binary on `$PATH` —
//!   host-dependence *was* the bug.
//! - The refusal is a panic, not a substitution. Silently swapping in the stub
//!   would hide exactly the mistakes this fence exists to surface.
//! - The allowance is a closed list, not a judgement about a given test. No
//!   caller may argue its way past the fence with "this one cannot really
//!   spawn"; that argument is what produced the incident.

use orgasmic_drivers::WorkerDriver;

/// Resolve a first-class `(mode, harness)` pair to a driver.
///
/// Production build: the registry lookup, unchanged.
#[cfg(not(test))]
pub(crate) fn resolve_driver(mode: &str, harness: &str) -> Option<Box<dyn WorkerDriver>> {
    orgasmic_drivers::driver_for_mode_harness(mode, harness)
}

/// Resolve a legacy transport id to a driver.
///
/// Production build: the registry lookup, unchanged.
#[cfg(not(test))]
pub(crate) fn resolve_driver_by_transport(transport: &str) -> Option<Box<dyn WorkerDriver>> {
    orgasmic_drivers::driver_for(transport)
}

/// The only real modes a daemon test build may resolve, and the only entry in
/// this file that is an exception rather than a rule.
///
/// `tmux` and `rmux` are multiplexers. They cannot exec a harness on their own:
/// what runs in the pane is the command the daemon writes into the driver
/// config, which a test controls outright (`tmux::inert_config`, a pinned
/// fixture executable). They are also the surface the suite's existing live
/// mux family is built on — `test_tooling::skip_test_if_missing`,
/// `own_rmux_server_for_tests`, `test_environment_lock` — a deliberate,
/// gated body of coverage that this fence is not entitled to delete.
///
/// Everything else — `acp-stdio`, `acp-ws`, `subprocess-stream-json`, and any
/// mode added later — execs the harness binary itself, resolved from `$PATH`,
/// with no pane and no gate between the test and a billed provider turn. Those
/// are refused, always.
///
/// This list is the fence's entire allowance. Adding to it means arguing that a
/// new mode cannot exec a provider binary, in writing, here.
#[cfg(test)]
pub(crate) const LIVE_TOOLING_MODES: &[&str] = &["tmux", "rmux"];

/// Legacy transport ids that name a [`LIVE_TOOLING_MODES`] transport.
#[cfg(test)]
const LIVE_TOOLING_TRANSPORTS: &[&str] = &["tmux-tui"];

/// Test profile: [`STUB_MODE`]/[`STUB_HARNESS`] and the mux modes resolve;
/// every other mode panics.
///
/// An address the registry does not know still answers `None`, so the callers'
/// "unsupported pair" rejections stay testable.
#[cfg(test)]
pub(crate) fn resolve_driver(mode: &str, harness: &str) -> Option<Box<dyn WorkerDriver>> {
    if (mode, harness) == (STUB_MODE, STUB_HARNESS) {
        return Some(stub_driver());
    }
    if let Some(refusal) = pair_refusal(mode, harness) {
        panic!("{refusal}");
    }
    orgasmic_drivers::driver_for_mode_harness(mode, harness)
}

/// Test profile: legacy transport ids name the same transports, so they are
/// refused on the same terms.
#[cfg(test)]
pub(crate) fn resolve_driver_by_transport(transport: &str) -> Option<Box<dyn WorkerDriver>> {
    if transport == STUB_TRANSPORT {
        return Some(stub_driver());
    }
    if let Some(refusal) = transport_refusal(transport) {
        panic!("{refusal}");
    }
    orgasmic_drivers::driver_for(transport)
}

#[cfg(test)]
pub(crate) use stub::{
    stub_config, stub_driver, STUB_HARNESS, STUB_MODE, STUB_TRANSPORT, TEST_PROFILE_REFUSAL,
};

/// The refusal a `(mode, harness)` pair earns under the test profile, or `None`
/// when the pair is resolvable there.
///
/// Pure: it reads the registry's static tables and this file's allowance, and
/// nothing else — no `$PATH`, no process spawn, no filesystem. That is what
/// makes the guard's answer identical on every host.
#[cfg(test)]
pub(crate) fn pair_refusal(mode: &str, harness: &str) -> Option<String> {
    if LIVE_TOOLING_MODES.contains(&mode) {
        return None;
    }
    orgasmic_drivers::SUPPORTED
        .contains(&(mode, harness))
        .then(|| refusal_message(&format!("{mode}/{harness}")))
}

/// The same refusal for a legacy transport id.
#[cfg(test)]
pub(crate) fn transport_refusal(transport: &str) -> Option<String> {
    if LIVE_TOOLING_TRANSPORTS.contains(&transport) {
        return None;
    }
    orgasmic_drivers::TRANSPORTS
        .contains(&transport)
        .then(|| refusal_message(transport))
}

#[cfg(test)]
fn refusal_message(named: &str) -> String {
    format!(
        "{TEST_PROFILE_REFUSAL} {named}: a daemon test must never hold a \
         transport that can exec a provider binary. Address the in-process stub \
         ({STUB_MODE}/{STUB_HARNESS}) instead — see \
         `crate::driver_resolution::stub_driver` (TASK-3NJ9K)."
    )
}

/// The in-process stub transport, and the fixtures that address it.
#[cfg(test)]
mod stub {
    use std::sync::Arc;

    use async_trait::async_trait;
    use orgasmic_core::DriverEvent;
    use orgasmic_drivers::{
        BabysitterAck, BabysitterRequest, DriverConfig, DriverContext, DriverControl, DriverError,
        DriverSession, TransitionAck, TransitionRequest, TransportInteraction, WorkerDriver,
    };
    use serde_json::json;
    use tokio::sync::mpsc::Sender;
    use tokio::sync::Mutex;

    /// Leading text of every test-profile resolution refusal. Tests assert on
    /// this prefix, so it is one constant rather than a repeated literal.
    pub(crate) const TEST_PROFILE_REFUSAL: &str =
        "orgasmic daemon test profile refuses to resolve the real transport";

    /// Mode id of the stub. Deliberately absent from `orgasmic_drivers::MODES`:
    /// nothing outside a test build can address it.
    pub(crate) const STUB_MODE: &str = "stub";
    /// Harness id of the stub.
    pub(crate) const STUB_HARNESS: &str = "stub";
    /// Legacy-transport spelling of the stub.
    pub(crate) const STUB_TRANSPORT: &str = "stub";

    /// Build the in-process stub transport.
    pub(crate) fn stub_driver() -> Box<dyn WorkerDriver> {
        Box::new(StubDriver::new())
    }

    /// The stub reads no configuration; this is the empty config every
    /// stub-addressed fixture passes.
    pub(crate) fn stub_config() -> DriverConfig {
        DriverConfig::from_value(json!({}))
    }

    /// A transport with no subprocess, no socket, and no host lookup.
    ///
    /// It reaches `Ready` on acquire and then stays silent, holding its sender
    /// open so the supervisor sees a live run rather than an immediate stream
    /// end. Release closes the channel. That is the whole behaviour a
    /// supervisor- or dispatch-level test needs from "a transport that
    /// started".
    pub(crate) struct StubDriver {
        event_tx: Arc<Mutex<Option<Sender<DriverEvent>>>>,
    }

    impl StubDriver {
        pub(crate) fn new() -> Self {
            Self {
                event_tx: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[async_trait]
    impl WorkerDriver for StubDriver {
        fn transport(&self) -> &'static str {
            STUB_MODE
        }

        fn harness(&self) -> Option<&'static str> {
            Some(STUB_HARNESS)
        }

        fn interaction(&self) -> TransportInteraction {
            TransportInteraction::Unattended
        }

        async fn acquire(
            &self,
            ctx: DriverContext,
            _config: DriverConfig,
        ) -> Result<DriverSession, DriverError> {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            *self.event_tx.lock().await = Some(tx.clone());
            let _ = tx
                .send(DriverEvent::Ready {
                    protocol_version: "stub/1".into(),
                    capabilities: json!({"stub": true}),
                })
                .await;
            Ok(DriverSession {
                identity: ctx.identity,
                pid: None,
                events: rx,
                control: Box::new(StubControl {
                    event_tx: Arc::clone(&self.event_tx),
                }),
                producer: None,
                native_runtime: None,
            })
        }
    }

    struct StubControl {
        event_tx: Arc<Mutex<Option<Sender<DriverEvent>>>>,
    }

    #[async_trait]
    impl DriverControl for StubControl {
        async fn transition_state(
            &mut self,
            req: TransitionRequest,
        ) -> Result<TransitionAck, DriverError> {
            Ok(TransitionAck {
                accepted: true,
                message: Some(format!("stub transition {} -> {}", req.from, req.to)),
            })
        }

        async fn babysitter_action(
            &mut self,
            req: BabysitterRequest,
        ) -> Result<BabysitterAck, DriverError> {
            Ok(BabysitterAck {
                accepted: true,
                message: Some(format!("stub babysitter {:?}", req.tool)),
            })
        }

        async fn release(&mut self, _reason: &str) -> Result<(), DriverError> {
            self.event_tx.lock().await.take();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pair on a harness-exec mode is refused, and the message names the
    /// pair and the stub. This is the fence itself, checked without going near
    /// a driver: `pair_refusal` is what `resolve_driver` panics with.
    ///
    /// Default-deny is the property under test: the loop asks the registry for
    /// its whole pair table rather than a list kept here, so a mode added to
    /// `orgasmic_drivers::MODES` tomorrow is refused the day it lands unless
    /// someone deliberately adds it to `LIVE_TOOLING_MODES`.
    #[test]
    fn every_harness_exec_pair_is_refused_under_the_test_profile() {
        let mut refused = 0;
        for (mode, harness) in orgasmic_drivers::SUPPORTED {
            if LIVE_TOOLING_MODES.contains(mode) {
                assert!(
                    pair_refusal(mode, harness).is_none(),
                    "{mode} is the fence's declared mux allowance"
                );
                continue;
            }
            let refusal = pair_refusal(mode, harness)
                .unwrap_or_else(|| panic!("{mode}/{harness} must be refused in a test build"));
            assert!(
                refusal.contains(&format!("{mode}/{harness}")),
                "the refusal must name the pair the test asked for: {refusal}"
            );
            assert!(
                refusal.contains("stub/stub"),
                "the refusal must point at the stub: {refusal}"
            );
            refused += 1;
        }
        assert!(
            refused >= 4,
            "the registry's harness-exec pairs must be reaching this loop; got {refused}"
        );
        for transport in orgasmic_drivers::TRANSPORTS {
            if LIVE_TOOLING_TRANSPORTS.contains(transport) {
                continue;
            }
            assert!(
                transport_refusal(transport).is_some(),
                "legacy transport {transport} must be refused in a test build"
            );
        }
    }

    /// The stub resolves, and it is not something the registry could ever hand
    /// out — nothing outside a test build can address it.
    #[test]
    fn the_stub_resolves_and_is_test_only() {
        let driver = resolve_driver(STUB_MODE, STUB_HARNESS).expect("the stub must resolve");
        assert_eq!(driver.transport(), STUB_MODE);
        assert_eq!(driver.harness(), Some(STUB_HARNESS));
        assert!(
            orgasmic_drivers::driver_for_mode_harness(STUB_MODE, STUB_HARNESS).is_none(),
            "the stub must be a test-profile transport, not a registry entry"
        );
        assert!(resolve_driver_by_transport(STUB_TRANSPORT).is_some());
    }

    /// A pair the registry does not know keeps answering `None`, so the
    /// callers' "unsupported pair" rejections stay reachable from a test.
    #[test]
    fn unknown_pairs_still_answer_none() {
        assert!(resolve_driver("acp-stdio", "not-a-harness").is_none());
        assert!(resolve_driver("not-a-mode", "claude").is_none());
        assert!(resolve_driver_by_transport("not-a-transport").is_none());
    }

    #[test]
    #[should_panic(expected = "refuses to resolve the real transport acp-stdio/hermes")]
    fn resolving_acp_stdio_hermes_panics() {
        let _ = resolve_driver("acp-stdio", "hermes");
    }

    /// The fence is only worth having if it is the *only* way into a
    /// harness-exec transport, so this asserts that shape directly: no file in
    /// the daemon outside this one names the registry lookups or constructs one
    /// of the harness-exec mode drivers by hand.
    ///
    /// Without it the guard is a convention, and the next author to write
    /// `use orgasmic_drivers::driver_for_mode_harness` — or
    /// `AcpStdioDriver::new(HermesAdapter::new())` — in a test walks straight
    /// past it, which is exactly the move that produced the incident. The mux
    /// drivers are absent from the list on purpose: they are this fence's
    /// declared allowance ([`LIVE_TOOLING_MODES`]).
    #[test]
    fn nothing_outside_this_module_reaches_the_driver_registry() {
        // `AcpWsDriver` without the `::` is `AcpWsDriverDefaults`, a config
        // struct; prose naming a driver is not a call. Both are matched out
        // rather than special-cased later.
        const FORBIDDEN: &[&str] = &[
            "driver_for",
            "AcpStdioDriver::",
            "AcpWsDriver::",
            "SubprocessStreamJsonDriver::",
        ];

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read daemon src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        assert!(files.len() > 5, "the daemon src scan found {files:?}");

        let mut offenders = Vec::new();
        for file in files {
            if file
                .file_name()
                .is_some_and(|name| name == "driver_resolution.rs")
            {
                continue;
            }
            let body = std::fs::read_to_string(&file).expect("read daemon source");
            for (idx, line) in body.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with('*') {
                    continue;
                }
                if let Some(name) = FORBIDDEN.iter().find(|name| line.contains(**name)) {
                    offenders.push(format!("{}:{} ({name})", file.display(), idx + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the driver registry may only be reached through \
             `crate::driver_resolution`; these call it directly and so bypass \
             the test-profile fence: {offenders:?}"
        );
    }

    #[test]
    #[should_panic(expected = "refuses to resolve the real transport acp-ws/codex")]
    fn resolving_acp_ws_codex_panics() {
        let _ = resolve_driver("acp-ws", "codex");
    }

    #[test]
    #[should_panic(expected = "refuses to resolve the real transport hermes")]
    fn resolving_legacy_hermes_transport_panics() {
        let _ = resolve_driver_by_transport("hermes");
    }
}
