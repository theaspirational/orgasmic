// orgasmic:dec_WDR5K
//! Content families a hard cutover retired, and the decision that retired each.
//!
//! A hard cutover (dec_WDR5K item 10 is the first) removes a concept from the
//! runtime but leaves the operator's files on disk. Those files then look
//! exactly like live configuration: an agent that finds
//! `~/.orgasmic/user/workers/*.org` reads plausible `:DEFAULT_MODEL:` values and
//! reasons confidently from them, because nothing on the path it reads says the
//! runtime stopped loading them. A stale file with plausible content is worse
//! than a missing one — it produces confident wrong reasoning rather than a
//! lookup failure.
//!
//! This table is the one place that knowledge lives. Everything that surfaces
//! retired content reads it: the daemon's boot warning, `orgasmic doctor`,
//! `orgasmic doctor --remove-retired`, and the `Retired content` section of the
//! shipped entry router. Adding a path here therefore lands in every surface in
//! the same release, which is the maintenance discipline the second-order
//! incident (TASK-8ED6V) asked for — a cutover author has one edit to make, not
//! four to remember.

use std::path::PathBuf;

use crate::home::Home;

/// One retired content family, addressed by its path under `$ORGASMIC_HOME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetiredContent {
    /// Path relative to `$ORGASMIC_HOME`, e.g. `user/workers`.
    pub rel_path: &'static str,
    /// Node id of the decision that retired it, so a reader can check the
    /// rationale instead of guessing: `orgasmic decision get <id>`.
    pub deciding_node: &'static str,
    /// What used to live there, in one line.
    pub summary: &'static str,
}

impl RetiredContent {
    /// Absolute location under this home.
    pub fn path(&self, home: &Home) -> PathBuf {
        home.root.join(self.rel_path)
    }

    /// Whether the operator still has this residue on disk.
    pub fn is_present(&self, home: &Home) -> bool {
        self.path(home).exists()
    }
}

/// Every content family retired by a hard cutover. Append here when a decision
/// retires one; do not remove entries, because the residue outlives the release
/// that stopped reading it.
pub const RETIRED_CONTENT: &[RetiredContent] = &[RetiredContent {
    rel_path: "user/workers",
    deciding_node: "dec_WDR5K",
    summary: "worker templates (per-worker .org files carrying transport, governance, \
              and :DEFAULT_MODEL:/:DEFAULT_EFFORT: overrides)",
}];

/// The retired families this home still has on disk, in table order.
pub fn present(home: &Home) -> Vec<&'static RetiredContent> {
    RETIRED_CONTENT
        .iter()
        .filter(|entry| entry.is_present(home))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_entry_is_a_relative_path_inside_home() {
        for entry in RETIRED_CONTENT {
            let rel = std::path::Path::new(entry.rel_path);
            assert!(
                rel.is_relative(),
                "{} must be relative to $ORGASMIC_HOME",
                entry.rel_path
            );
            assert!(
                !entry.rel_path.split('/').any(|part| part == ".."),
                "{} must not escape $ORGASMIC_HOME",
                entry.rel_path
            );
            assert!(
                !entry.rel_path.trim().is_empty(),
                "retired entry has an empty path"
            );
        }
    }

    /// The check names the deciding node, not just the path — a reader who
    /// disbelieves the finding has to be able to look the rationale up.
    #[test]
    fn every_entry_names_a_resolvable_deciding_node() {
        for entry in RETIRED_CONTENT {
            assert!(
                crate::id::is_dec_id(entry.deciding_node),
                "{} names `{}` as its deciding node, which is not a decision id",
                entry.rel_path,
                entry.deciding_node
            );
            assert!(
                !entry.summary.trim().is_empty(),
                "{} has no summary, so a reader learns only that a path is dead",
                entry.rel_path
            );
        }
    }

    #[test]
    fn paths_are_unique() {
        let unique: BTreeSet<&str> = RETIRED_CONTENT.iter().map(|entry| entry.rel_path).collect();
        assert_eq!(
            unique.len(),
            RETIRED_CONTENT.len(),
            "duplicate retired paths would emit duplicate findings"
        );
    }

    #[test]
    fn present_reports_only_residue_that_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.ensure().unwrap();

        assert!(present(&home).is_empty());

        std::fs::create_dir_all(home.root.join("user/workers")).unwrap();
        let found = present(&home);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rel_path, "user/workers");
        assert_eq!(found[0].deciding_node, "dec_WDR5K");
    }
}
