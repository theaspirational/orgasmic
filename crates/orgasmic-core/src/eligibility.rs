//! HAR-style worker/task eligibility matching.

use serde::Serialize;

use crate::schema::WorkerKind;
use crate::workers::Worker;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkerEligibility {
    pub worker_id: String,
    pub eligible: bool,
    pub effective_provider: Option<String>,
    pub reasons: Vec<String>,
}

/// A read-only view of the task fields that affect worker selection.
#[derive(Debug, Clone)]
pub struct TaskConstraints<'a> {
    pub kind: WorkerKind,
    pub provider: Option<&'a str>,
}

pub fn list_allows(list: &[String], value: &str) -> bool {
    list.is_empty()
        || list
            .iter()
            .any(|candidate| candidate == "*" || candidate == value)
}

pub fn compute_eligibility(task: &TaskConstraints<'_>, worker: &Worker<'_>) -> WorkerEligibility {
    let mut reasons = Vec::new();
    let effective_provider = normalize(task.provider).or_else(|| worker.default_provider.clone());

    if worker.kind != task.kind {
        reasons.push(format!(
            "task kind mismatch (task {}, worker {})",
            worker_kind_name(task.kind),
            worker_kind_name(worker.kind)
        ));
    }

    // A `custom` harness wraps an opaque operator CLI (`:HARNESS_ARGS:` is the
    // whole command line); orgasmic cannot pick its provider, so an unpinned
    // provider is not a failure the way it is for typed harnesses.
    let opaque_harness = worker.harness == "custom";

    match &effective_provider {
        Some(provider) if list_allows(&worker.providers, provider) => {}
        Some(provider) => reasons.push(format!("provider not supported: {provider}")),
        None if opaque_harness => {}
        None => reasons.push(
            "no effective provider (task omitted :PROVIDER: and worker has no DEFAULT_PROVIDER)"
                .to_string(),
        ),
    }

    WorkerEligibility {
        worker_id: worker.id.to_string(),
        eligible: reasons.is_empty(),
        effective_provider,
        reasons,
    }
}

/// Returns the eligibility outcome for each worker, in the input order.
pub fn compute_all<'a>(
    task: &TaskConstraints<'_>,
    workers: &'a [Worker<'a>],
) -> Vec<WorkerEligibility> {
    workers
        .iter()
        .map(|worker| compute_eligibility(task, worker))
        .collect()
}

fn normalize(value: Option<&str>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_ascii_lowercase())
        .filter(|trimmed| !trimmed.is_empty())
}

fn worker_kind_name(kind: WorkerKind) -> &'static str {
    match kind {
        WorkerKind::Implementer => "implementer",
        WorkerKind::Reviewer => "reviewer",
        WorkerKind::Planner => "planner",
        WorkerKind::Analyzer => "analyzer",
        WorkerKind::Architector => "architector",
        WorkerKind::Griller => "griller",
        WorkerKind::Glossarist => "glossarist",
        WorkerKind::Babysitter => "babysitter",
        WorkerKind::Manager => "manager",
        WorkerKind::Artifactor => "artifactor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> Worker<'static> {
        Worker {
            id: "w1",
            kind: WorkerKind::Implementer,
            driver: "tmux",
            harness: "codex",
            providers: vec!["openai".to_string()],
            default_provider: Some("openai".to_string()),
            linked_skills: Vec::new(),
            applicable_states: Vec::new(),
            max_iterations: None,
            context_budget_chars: None,
            stall_timeout_secs: None,
            max_run_duration_secs: None,
            babysitter_worker: None,
            sandbox_permissions: None,
            harness_args: Vec::new(),
            version: None,
            persona: None,
            operating_rules: None,
        }
    }

    fn task() -> TaskConstraints<'static> {
        TaskConstraints {
            kind: WorkerKind::Implementer,
            provider: None,
        }
    }

    #[test]
    fn compute_eligibility_uses_task_provider_pin() {
        let mut task = task();
        task.provider = Some("openai");

        let got = compute_eligibility(&task, &worker());

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn compute_eligibility_falls_back_to_worker_default_provider_when_task_unpinned() {
        let got = compute_eligibility(&task(), &worker());

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn compute_eligibility_rejects_when_capability_list_missing_value() {
        let mut worker = worker();
        worker.providers = vec!["anthropic".to_string()];
        let mut task = task();
        task.provider = Some("openai");

        let got = compute_eligibility(&task, &worker);

        assert!(!got.eligible);
        assert!(got
            .reasons
            .contains(&"provider not supported: openai".to_string()));
    }

    #[test]
    fn compute_eligibility_rejects_on_kind_mismatch() {
        let mut task = task();
        task.kind = WorkerKind::Reviewer;

        let got = compute_eligibility(&task, &worker());

        assert!(!got.eligible);
        assert!(got
            .reasons
            .iter()
            .any(|reason| reason.contains("task kind mismatch")));
    }

    #[test]
    fn compute_eligibility_empty_capability_list_allows_anything() {
        let mut worker = worker();
        worker.providers = Vec::new();
        let mut task = task();
        task.provider = Some("anthropic");

        let got = compute_eligibility(&task, &worker);

        assert!(got.eligible, "{:?}", got.reasons);
    }

    #[test]
    fn compute_eligibility_custom_harness_needs_no_provider() {
        let mut worker = worker();
        worker.harness = "custom";
        worker.providers = Vec::new();
        worker.default_provider = None;
        worker.harness_args = vec!["opencode".to_string()];

        let got = compute_eligibility(&task(), &worker);

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider, None);
    }

    #[test]
    fn compute_eligibility_lowercases_and_trims_provider() {
        let mut task = task();
        task.provider = Some(" OpenAI ");

        let got = compute_eligibility(&task, &worker());

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider.as_deref(), Some("openai"));
    }
}
