//! Compact, time-sortable run identity generation.
//!
//! New run ids are `run-` plus one monotonic ULID. Historical
//! `run-<timestamp>-<uuid>` values remain opaque, valid identifiers everywhere
//! else; only helpers that need the compact representation opt into parsing it.

use std::sync::{Mutex, OnceLock};

use ulid::{Generator, Ulid, ULID_LEN};

pub const RUN_ID_PREFIX: &str = "run-";

static RUN_ID_GENERATOR: OnceLock<Mutex<Generator>> = OnceLock::new();

/// Mint one process-monotonic, lexicographically time-sortable run id.
pub fn mint_run_id() -> String {
    let generator = RUN_ID_GENERATOR.get_or_init(|| Mutex::new(Generator::new()));
    let mut generator = generator
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let value = match generator.generate() {
        Ok(value) => value,
        // Exhausting all 80 random bits within one millisecond is not realistic,
        // but incrementing into the next millisecond preserves uniqueness and
        // ordering instead of making identity generation fallible.
        Err(overflow) => overflow.commit_overflow_increment(),
    };
    format!("{RUN_ID_PREFIX}{value}")
}

/// Return the canonical ULID token for a new-format run id.
///
/// Legacy ids intentionally return `None`; callers can then retain their exact
/// historical naming or parsing behavior.
pub fn compact_run_id_token(run_id: &str) -> Option<&str> {
    let token = run_id.strip_prefix(RUN_ID_PREFIX)?;
    if token.len() != ULID_LEN || Ulid::from_string(token).is_err() {
        return None;
    }
    Some(token)
}

/// Decode a new-format run id's embedded Unix timestamp in milliseconds.
pub fn run_id_timestamp_millis(run_id: &str) -> Option<u64> {
    compact_run_id_token(run_id)
        .and_then(|token| Ulid::from_string(token).ok())
        .map(|value| value.timestamp_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_ids_are_compact_valid_and_monotonic() {
        let first = mint_run_id();
        let second = mint_run_id();

        assert_eq!(first.len(), RUN_ID_PREFIX.len() + ULID_LEN);
        assert!(compact_run_id_token(&first).is_some());
        assert!(first < second);
    }

    #[test]
    fn compact_parser_rejects_historical_and_malformed_ids() {
        assert_eq!(
            compact_run_id_token("run-20260815T204602-0c52fd069d094384a3ab774439b2b4a1"),
            None
        );
        assert_eq!(compact_run_id_token("run-not-a-ulid"), None);
    }

    #[test]
    fn timestamp_is_embedded_in_the_same_identifier() {
        let run_id = "run-01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert_eq!(run_id_timestamp_millis(run_id), Some(1_469_922_850_259));
    }
}
