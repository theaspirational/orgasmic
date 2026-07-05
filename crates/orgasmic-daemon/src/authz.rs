// arch: arch_Z8CW2
// orgasmic:arch_Z8CW2, dec_KF2MR
//! The authorization seam: role -> capability table, grant resolution, and
//! the one [`require`] function every gated route/WS handler calls through.
//! Route and WS handler code never tests role names directly — only
//! [`Action`] variants and this module's static capability table know what a
//! role can do. Adding a role is a `role_capabilities` edit only.

use std::collections::HashSet;

use crate::events::{EventPayload, Topic};

/// Namespaced action vocabulary spanning the whole daemon surface (dec_KF2MR).
/// `MembersManage` has no v1 HTTP route (member management is CLI/host-local
/// only) but is named here so the vocabulary matches the decision record and
/// a future route is a capability-table lookup away, not new plumbing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ProjectRead,
    GraphRead,
    TasksRead,
    SessionsWatch,
    SessionsInteract,
    ArtifactsRead,
    ArtifactsComment,
    ArtifactsGenerate,
    #[allow(dead_code)]
    MembersManage,
}

impl Action {
    pub const ALL: [Action; 9] = [
        Action::ProjectRead,
        Action::GraphRead,
        Action::TasksRead,
        Action::SessionsWatch,
        Action::SessionsInteract,
        Action::ArtifactsRead,
        Action::ArtifactsComment,
        Action::ArtifactsGenerate,
        Action::MembersManage,
    ];
}

/// Dotted action name matching the vocabulary named in arch_Z8CW2/dec_KF2MR
/// (`graph.read`, `artifacts.generate`, …) — used by the `/me` capability
/// snapshot.
pub fn action_name(action: Action) -> &'static str {
    match action {
        Action::ProjectRead => "project.read",
        Action::GraphRead => "graph.read",
        Action::TasksRead => "tasks.read",
        Action::SessionsWatch => "sessions.watch",
        Action::SessionsInteract => "sessions.interact",
        Action::ArtifactsRead => "artifacts.read",
        Action::ArtifactsComment => "artifacts.comment",
        Action::ArtifactsGenerate => "artifacts.generate",
        Action::MembersManage => "members.manage",
    }
}

/// v1 built-in roles, one static table (dec_KF2MR). `ProjectRead` is an
/// addition beyond the decision's literal action list: every built-in role
/// needs *some* basis for "this project is visible to me at all" (board
/// filtering, `/me`), so it is included in all three rather than special-cased
/// as an implicit any-grant check outside the table. Public so the `/me`
/// handler can list a resolved role's capabilities without duplicating the
/// table or testing role names itself.
pub fn role_capabilities(role: &str) -> &'static [Action] {
    use Action::*;
    match role {
        "viewer" => &[
            ProjectRead,
            GraphRead,
            TasksRead,
            SessionsWatch,
            ArtifactsRead,
            ArtifactsComment,
        ],
        "editor" => &[
            ProjectRead,
            GraphRead,
            TasksRead,
            SessionsWatch,
            ArtifactsRead,
            ArtifactsComment,
            ArtifactsGenerate,
        ],
        "artifacts" => &[ProjectRead, ArtifactsRead, ArtifactsComment],
        _ => &[],
    }
}

/// Resolved request identity. `Admin` is the pre-existing single daemon
/// bearer token (or the admin UI session minted from it) — unchanged, full
/// access, never role-checked. `Member` carries the grants read fresh from
/// `members.org` at session-resolution time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Admin,
    Member {
        name: String,
        /// `(project-or-*, role)` pairs, in `members.org` file order.
        grants: Vec<(String, String)>,
    },
}

impl Identity {
    pub fn member_name(&self) -> Option<&str> {
        match self {
            Identity::Admin => None,
            Identity::Member { name, .. } => Some(name),
        }
    }

    /// Resolve which role (if any) a member holds for `project`. Exact
    /// project match beats `*`; no matching grant means no access. `Admin`
    /// always resolves to `None` here — admin bypasses role resolution
    /// entirely in [`require`]; callers building a display/snapshot for an
    /// admin identity should show `"admin"` directly rather than calling this.
    pub fn role_for(&self, project: &str) -> Option<&str> {
        let Identity::Member { grants, .. } = self else {
            return None;
        };
        grants
            .iter()
            .find(|(p, _)| p == project)
            .or_else(|| grants.iter().find(|(p, _)| p == "*"))
            .map(|(_, role)| role.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Forbidden(pub String);

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Forbidden {}

/// The one authorization seam. Every gated route/WS handler resolves its
/// decision through this call — never by comparing role strings itself.
/// `project` is `None` only for actions that are never project-scoped; no
/// action in the v1 member vocabulary qualifies, so a member identity with
/// `project: None` always fails closed.
pub fn require(identity: &Identity, project: Option<&str>, action: Action) -> Result<(), Forbidden> {
    if matches!(identity, Identity::Admin) {
        return Ok(());
    }
    let Some(project) = project else {
        return Err(Forbidden("action requires a project".into()));
    };
    let Some(role) = identity.role_for(project) else {
        return Err(Forbidden(format!("no grant for project {project}")));
    };
    if role_capabilities(role).contains(&action) {
        Ok(())
    } else {
        Err(Forbidden(format!("role {role} lacks {action:?}")))
    }
}

/// Filter `all` project ids down to the ones `identity` may see at all (board
/// listing / `/me` — "the project absent from that member's UI project
/// list", dec_KF2MR). Admin sees everything; a member sees only projects with
/// some grant (exact or `*`).
pub fn visible_project_ids<'a, I>(identity: &Identity, all: I) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    match identity {
        Identity::Admin => all.into_iter().collect(),
        Identity::Member { grants, .. } => {
            let wildcard = grants.iter().any(|(p, _)| p == "*");
            all.into_iter()
                .filter(|id| wildcard || grants.iter().any(|(p, _)| p == id))
                .collect()
        }
    }
}

/// Coarse WS topic eligibility (dec_KF2MR: "an `artifacts` member's socket
/// only receives artifact.* events"). Per-event project scoping is layered on
/// top by [`event_visible`] when the payload carries a `project_id`.
pub fn allowed_topics(identity: &Identity) -> HashSet<Topic> {
    match identity {
        Identity::Admin => Topic::ALL.into_iter().collect(),
        Identity::Member { grants, .. } => {
            let mut topics = HashSet::new();
            for (_, role) in grants {
                let caps = role_capabilities(role);
                if caps.contains(&Action::ArtifactsRead) {
                    topics.insert(Topic::Artifact);
                }
                if caps.contains(&Action::GraphRead) {
                    topics.insert(Topic::Graph);
                    topics.insert(Topic::Board);
                }
                if caps.contains(&Action::TasksRead) {
                    topics.insert(Topic::Task);
                    topics.insert(Topic::Board);
                }
            }
            topics
        }
    }
}

/// Whether `identity` may receive one event: topic-eligible, and — when the
/// payload names a `project_id` — grant-eligible for that specific project.
/// Payloads with no `project_id` (board refresh, daemon heartbeat, …) pass
/// once their topic is allowed; this is the "coarse" filter dec_KF2MR
/// deliberately chose over per-event fine-grained filtering (deferred).
pub fn event_visible(identity: &Identity, topic: Topic, payload: &EventPayload) -> bool {
    if !allowed_topics(identity).contains(&topic) {
        return false;
    }
    match (identity, payload.project_id()) {
        (Identity::Admin, _) => true,
        (Identity::Member { .. }, None) => true,
        (Identity::Member { grants, .. }, Some(project)) => {
            grants.iter().any(|(p, _)| p == project || p == "*")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(grants: &[(&str, &str)]) -> Identity {
        Identity::Member {
            name: "alice".into(),
            grants: grants
                .iter()
                .map(|(p, r)| (p.to_string(), r.to_string()))
                .collect(),
        }
    }

    #[test]
    fn admin_bypasses_every_check() {
        assert!(require(&Identity::Admin, None, Action::MembersManage).is_ok());
        assert!(require(&Identity::Admin, Some("proj-a"), Action::ArtifactsGenerate).is_ok());
    }

    #[test]
    fn exact_project_grant_beats_wildcard() {
        let id = member(&[("proj-a", "viewer"), ("*", "editor")]);
        // proj-a: exact viewer grant wins, so generate (editor-only) is denied.
        assert!(require(&id, Some("proj-a"), Action::ArtifactsGenerate).is_err());
        assert!(require(&id, Some("proj-a"), Action::ArtifactsRead).is_ok());
        // proj-b: falls back to the wildcard editor grant.
        assert!(require(&id, Some("proj-b"), Action::ArtifactsGenerate).is_ok());
    }

    #[test]
    fn no_matching_grant_is_forbidden() {
        let id = member(&[("proj-a", "editor")]);
        assert!(require(&id, Some("proj-b"), Action::ArtifactsRead).is_err());
    }

    #[test]
    fn viewer_lacks_generate_editor_has_it() {
        let viewer = member(&[("*", "viewer")]);
        let editor = member(&[("*", "editor")]);
        assert!(require(&viewer, Some("p"), Action::ArtifactsGenerate).is_err());
        assert!(require(&editor, Some("p"), Action::ArtifactsGenerate).is_ok());
        assert!(require(&viewer, Some("p"), Action::TasksRead).is_ok());
    }

    #[test]
    fn artifacts_role_is_scoped_to_artifacts_only() {
        let id = member(&[("*", "artifacts")]);
        assert!(require(&id, Some("p"), Action::ArtifactsRead).is_ok());
        assert!(require(&id, Some("p"), Action::ArtifactsComment).is_ok());
        assert!(require(&id, Some("p"), Action::GraphRead).is_err());
        assert!(require(&id, Some("p"), Action::TasksRead).is_err());
        assert!(require(&id, Some("p"), Action::SessionsWatch).is_err());
    }

    #[test]
    fn unknown_role_grants_nothing() {
        let id = member(&[("*", "made-up-role")]);
        assert!(require(&id, Some("p"), Action::ArtifactsRead).is_err());
    }

    #[test]
    fn member_action_without_project_always_fails() {
        let id = member(&[("*", "editor")]);
        assert!(require(&id, None, Action::ArtifactsRead).is_err());
    }

    #[test]
    fn visible_projects_filters_to_grants_admin_sees_all() {
        let id = member(&[("proj-a", "viewer")]);
        let all = ["proj-a", "proj-b", "proj-c"];
        assert_eq!(visible_project_ids(&id, all), vec!["proj-a"]);
        assert_eq!(
            visible_project_ids(&Identity::Admin, all),
            vec!["proj-a", "proj-b", "proj-c"]
        );
    }

    #[test]
    fn visible_projects_wildcard_sees_everything() {
        let id = member(&[("*", "viewer")]);
        let all = ["proj-a", "proj-b"];
        assert_eq!(visible_project_ids(&id, all), vec!["proj-a", "proj-b"]);
    }

    #[test]
    fn allowed_topics_artifacts_role_is_artifact_only() {
        let id = member(&[("*", "artifacts")]);
        let topics = allowed_topics(&id);
        assert_eq!(topics, HashSet::from([Topic::Artifact]));
    }

    #[test]
    fn allowed_topics_viewer_gets_board_task_graph_artifact() {
        let id = member(&[("*", "viewer")]);
        let topics = allowed_topics(&id);
        assert_eq!(
            topics,
            HashSet::from([Topic::Board, Topic::Task, Topic::Graph, Topic::Artifact])
        );
    }

    #[test]
    fn allowed_topics_admin_gets_everything() {
        assert_eq!(
            allowed_topics(&Identity::Admin),
            Topic::ALL.into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn event_visible_checks_topic_then_project() {
        let id = member(&[("proj-a", "artifacts")]);
        let in_scope = EventPayload::ArtifactChanged {
            project_id: "proj-a".into(),
            artifact_id: "ART-1".into(),
            state: "submitted".into(),
        };
        let out_of_scope = EventPayload::ArtifactChanged {
            project_id: "proj-b".into(),
            artifact_id: "ART-2".into(),
            state: "submitted".into(),
        };
        assert!(event_visible(&id, Topic::Artifact, &in_scope));
        assert!(!event_visible(&id, Topic::Artifact, &out_of_scope));
        // Wrong topic for the payload's own kind: still gated correctly.
        assert!(!event_visible(&id, Topic::Task, &in_scope));
    }

    #[test]
    fn event_visible_passes_project_less_payloads_when_topic_allowed() {
        let id = member(&[("proj-a", "viewer")]);
        assert!(event_visible(&id, Topic::Board, &EventPayload::BoardRefreshed));
        assert!(!event_visible(&id, Topic::Daemon, &EventPayload::DaemonHeartbeat));
    }
}
