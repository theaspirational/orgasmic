// arch: arch_WZFAX.1
// orgasmic:arch_WZFAX, arch_C87Z9, dec_XSV21, dec_N17XX
//! Re-export for the shared `Home` layout lifted into orgasmic-core.
//!
//! The CLI used to own `Home::from_env`, `Home::ensure`, and
//! `resolve_loader`. TASK-005 lifts them into `orgasmic-core::home` so the
//! daemon and CLI share one implementation and one set of layout invariants.
//! The CLI keeps this thin module as a re-export to minimize diffs in the
//! rest of the CLI.

pub use orgasmic_core::home::Home;
