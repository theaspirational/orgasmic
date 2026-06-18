//! HAR-style worker/task eligibility matching.

use serde::Serialize;

use crate::schema::WorkerKind;
use crate::workers::Worker;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkerEligibility {
    pub worker_id: String,
    pub eligible: bool,
    pub effective_provider: Option<String>,
    pub effective_model: Option<String>,
    pub effective_effort: Option<String>,
    pub reasons: Vec<String>,
}

/// A read-only view of the task fields that affect worker selection.
#[derive(Debug, Clone)]
pub struct TaskConstraints<'a> {
    pub kind: WorkerKind,
    pub provider: Option<&'a str>,
    pub model: Option<&'a str>,
    pub reasoning_effort: Option<&'a str>,
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
    let effective_model = normalize(task.model).or_else(|| worker.default_model.clone());
    let effective_effort =
        normalize(task.reasoning_effort).or_else(|| worker.default_effort.clone());

    if worker.kind != task.kind {
        reasons.push(format!(
            "task kind mismatch (task {}, worker {})",
            worker_kind_name(task.kind),
            worker_kind_name(worker.kind)
        ));
    }

    // A `custom` harness wraps an opaque operator CLI (`:HARNESS_ARGS:` is the
    // whole command line); orgasmic cannot pick its provider/model, so an
    // unpinned provider/model is not a failure the way it is for typed
    // harnesses. Explicit pins still have to match the capability lists.
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

    match &effective_model {
        Some(model) if list_allows(&worker.models, model) => {}
        Some(model) => reasons.push(format!("model not supported: {model}")),
        None if opaque_harness => {}
        None => reasons.push(
            "no effective model (task omitted :MODEL: and worker has no DEFAULT_MODEL)".to_string(),
        ),
    }

    if let Some(effort) = &effective_effort {
        if !worker.reasoning_efforts.is_empty() && !list_allows(&worker.reasoning_efforts, effort) {
            reasons.push(format!("reasoning effort not supported: {effort}"));
        }
    }

    WorkerEligibility {
        worker_id: worker.id.to_string(),
        eligible: reasons.is_empty(),
        effective_provider,
        effective_model,
        effective_effort,
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
            models: vec!["gpt-5".to_string(), "gpt-5.5".to_string()],
            reasoning_efforts: vec!["low".to_string(), "medium".to_string()],
            default_provider: Some("openai".to_string()),
            default_model: Some("gpt-5".to_string()),
            default_effort: Some("medium".to_string()),
            linked_skills: Vec::new(),
            applicable_states: Vec::new(),
            max_iterations: None,
            context_budget: None,
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
            model: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn compute_eligibility_uses_task_pin_over_worker_default() {
        let mut task = task();
        task.model = Some("gpt-5.5");

        let got = compute_eligibility(&task, &worker());

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn compute_eligibility_falls_back_to_worker_default_when_task_unpinned() {
        let got = compute_eligibility(&task(), &worker());

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider.as_deref(), Some("openai"));
        assert_eq!(got.effective_model.as_deref(), Some("gpt-5"));
        assert_eq!(got.effective_effort.as_deref(), Some("medium"));
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
    fn compute_eligibility_star_in_list_allows_anything() {
        let mut worker = worker();
        worker.models = vec!["*".to_string()];
        let mut task = task();
        task.model = Some("gpt-99");

        let got = compute_eligibility(&task, &worker);

        assert!(got.eligible, "{:?}", got.reasons);
    }

    #[test]
    fn compute_eligibility_unpinned_effort_is_not_a_failure() {
        let mut worker = worker();
        worker.default_effort = None;
        worker.reasoning_efforts = Vec::new();

        let got = compute_eligibility(&task(), &worker);

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_effort, None);
    }

    #[test]
    fn compute_eligibility_pinned_effort_rejected_when_not_listed() {
        let mut task = task();
        task.reasoning_effort = Some("xhigh");

        let got = compute_eligibility(&task, &worker());

        assert!(!got.eligible);
        assert!(got
            .reasons
            .contains(&"reasoning effort not supported: xhigh".to_string()));
    }

    #[test]
    fn compute_eligibility_custom_harness_needs_no_provider_or_model() {
        let mut worker = worker();
        worker.harness = "custom";
        worker.providers = Vec::new();
        worker.models = Vec::new();
        worker.reasoning_efforts = Vec::new();
        worker.default_provider = None;
        worker.default_model = None;
        worker.default_effort = None;
        worker.harness_args = vec!["opencode".to_string()];

        let got = compute_eligibility(&task(), &worker);

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider, None);
        assert_eq!(got.effective_model, None);
    }

    #[test]
    fn compute_eligibility_custom_harness_still_rejects_unlisted_pin() {
        let mut worker = worker();
        worker.harness = "custom";
        worker.models = vec!["composer-3".to_string()];
        worker.default_model = None;
        let mut task = task();
        task.model = Some("gpt-99");

        let got = compute_eligibility(&task, &worker);

        assert!(!got.eligible);
        assert!(got
            .reasons
            .contains(&"model not supported: gpt-99".to_string()));
    }

    #[test]
    fn compute_eligibility_lowercases_and_trims() {
        let mut task = task();
        task.provider = Some(" OpenAI ");

        let got = compute_eligibility(&task, &worker());

        assert!(got.eligible, "{:?}", got.reasons);
        assert_eq!(got.effective_provider.as_deref(), Some("openai"));
    }
}
