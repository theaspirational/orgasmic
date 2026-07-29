//! Print the production release-drain budget in milliseconds, and nothing else.
//!
//! orgasmic:TASK-HAREX — the window after which a run's event drain stops
//! waiting for a driver stream that has not ended since the release was
//! requested. Every test of that bound compresses it to a few hundred
//! milliseconds, because no test can afford to sit through the real one; this
//! example is what keeps the real one checkable without transcribing it.
//!
//! It prints `ShutdownBudgets::release_drain`, not a literal, and that is
//! deliberately the same expression the daemon installs on the supervisor at
//! boot — so the number a reader sees here and the number a wedged release
//! actually spends cannot drift apart.

fn main() {
    println!(
        "{}",
        orgasmic_daemon::ShutdownBudgets::default()
            .release_drain
            .as_millis()
    );
}
