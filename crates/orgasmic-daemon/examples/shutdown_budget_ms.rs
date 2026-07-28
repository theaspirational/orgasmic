//! Print `ShutdownBudgets::default().total()` in milliseconds, and nothing else.
//!
//! orgasmic:TASK-WE0Q7 — `scripts/soak.sh` asserts that a SIGTERMed daemon exits
//! within its shutdown budget plus a margin. The R74E8/ATAXN/QRB8S standard is
//! that such a bound is *derived* from the tree under test, never a literal
//! copied into a second place where it can drift: a soak still asserting 40s
//! against a tree whose budgets have grown to 90s is a gate that has silently
//! stopped bounding anything.
//!
//! A shell script cannot read a Rust constant, and nothing on the daemon's wire
//! surface exposes the sum. This example is the smallest bridge that keeps the
//! number compiled truth rather than a transcription: it prints exactly what the
//! shutdown path will spend, from the same expression the shutdown path uses.
//!
//! The soak also derives its default duration from this value (>= 10x), so both
//! numbers move together when a budget changes.

fn main() {
    println!(
        "{}",
        orgasmic_daemon::ShutdownBudgets::default()
            .total()
            .as_millis()
    );
}
