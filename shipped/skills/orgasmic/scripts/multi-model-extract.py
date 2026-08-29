#!/usr/bin/env python3
"""Compose existing orgasmic verbs into a multi-model knowledge run."""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


ARTIFACT_ID = re.compile(r"\bART-[0-9A-HJKMNP-TV-Z]{5}\b")
STARTED_TX = re.compile(r"\bstarted_tx=([^\s]+)")


@dataclass(frozen=True)
class Participant:
    mode: str
    harness: str
    dispatch_model: str
    effort: str
    vendor: str
    model: str

    @property
    def identity(self) -> str:
        return f"{self.harness} · {self.vendor} · {self.model} · effort {self.effort}"


@dataclass
class Dispatch:
    task: str
    started_tx: str
    participant: Participant
    closed: bool = False


class CommandError(RuntimeError):
    def __init__(self, args: list[str], result: subprocess.CompletedProcess[str]):
        self.returncode = result.returncode
        self.stdout = result.stdout
        self.stderr = result.stderr
        rendered = shlex.join(args[:8])
        super().__init__(
            f"command failed ({result.returncode}): {rendered}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )


class WaitUnknown(RuntimeError):
    """The watcher lost daemon contact, so worker liveness is not known."""


def command(args: list[str], *, cwd: Path | None = None, timeout: int | None = None) -> str:
    result = subprocess.run(
        args,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )
    if result.returncode:
        raise CommandError(args, result)
    return result.stdout


def parse_participant(raw: str) -> Participant:
    fields = [field.strip() for field in raw.split(",")]
    if len(fields) != 4 or not all(fields):
        raise ValueError("participant must be mode,harness,model,effort")
    if any("\n" in field or "\r" in field or "·" in field for field in fields):
        raise ValueError("participant fields must be single-line values without `·`")
    mode, harness, dispatch_model, effort = fields
    if "/" in dispatch_model:
        vendor, model = dispatch_model.split("/", 1)
    elif harness == "codex":
        vendor, model = "openai", dispatch_model
    elif harness == "claude":
        vendor, model = "anthropic", dispatch_model
    else:
        raise ValueError(
            f"cannot derive vendor for {harness}/{dispatch_model}; use provider/model"
        )
    return Participant(mode, harness, dispatch_model, effort, vendor, model)


def validate_participants(participants: list[Participant]) -> None:
    if len(participants) < 2:
        raise ValueError("at least two participants are required")
    models = [(p.vendor, p.model) for p in participants]
    if len(set(models)) != len(models):
        raise ValueError("participants must use different vendor/model identities")
    catalog = json.loads(
        command(
            [
                "orgasmic",
                "manager",
                "drivers",
                "--json",
                "--unattended-only",
                "--no-runtime-options",
            ]
        )
    )
    available = {
        (item["mode"], item["harness"])
        for item in catalog["transports"]
        if item.get("installed") and item.get("interaction") == "unattended"
    }
    missing = sorted({(p.mode, p.harness) for p in participants} - available)
    if missing:
        raise ValueError(f"unsupported or unavailable unattended transports: {missing}")


def project_context(project_override: str | None) -> tuple[str, Path, str]:
    entry = command(["orgasmic", "entry"])
    match = re.search(r"^PROJECT (.+)$", entry, re.MULTILINE)
    if not match:
        raise RuntimeError("orgasmic entry did not resolve a project")
    ledger = Path(match.group(1)).resolve()
    source = (ledger / ".orgasmic/project.org").read_text()
    project_match = re.search(r"^:ID:\s+(\S+)\s*$", source, re.MULTILINE)
    if not project_match:
        raise RuntimeError(f"project id missing from {ledger}/.orgasmic/project.org")
    project = project_match.group(1)
    if project_override and project_override != project:
        raise ValueError(
            f"--project {project_override} does not match current orgasmic project {project}"
        )
    default_branch = "main"
    for line in command(["orgasmic", "project", "list"]).splitlines():
        columns = line.split()
        if len(columns) >= 2 and columns[0] == project:
            default_branch = columns[1]
            break
    return project, ledger, default_branch


def compile_prompt(project: str, spec: str, values: dict[str, str], cwd: Path) -> str:
    args = ["orgasmic", "prompt", "compile", spec, "--project", project]
    for key, value in values.items():
        args.extend(["--value", f"{key}={value}"])
    compiled = json.loads(command(args, cwd=cwd))
    errors = [
        diag["message"]
        for diag in compiled.get("diagnostics", [])
        if diag.get("level") == "error"
    ]
    if errors:
        raise RuntimeError(f"{spec} prompt did not compile cleanly: {'; '.join(errors)}")
    return compiled["text"]


def create_task(
    project: str,
    ledger: Path,
    task_id: str,
    title: str,
    description: str,
    acceptance: str,
    *,
    read_scope: str,
    write_scope: str,
) -> None:
    body = (
        f"** Description\n{description}\n\n"
        f"** Acceptance Criteria\n- [ ] {acceptance}\n"
    )
    command(
        [
            "orgasmic",
            "task",
            "create",
            "--project",
            project,
            "--id",
            task_id,
            "--title",
            title,
            "--body",
            body,
            "--property",
            f"READ_SCOPE={read_scope}",
            "--property",
            f"WRITE_SCOPE={write_scope}",
            "--reason",
            "multi-model knowledge extraction run",
        ],
        cwd=ledger,
    )


def launch(
    ledger: Path,
    task: str,
    participant: Participant,
    brief: Path,
    source_ref: str,
    branch: str,
    reason: str,
) -> Dispatch:
    output = command(
        [
            "orgasmic",
            "manager",
            "dispatch",
            "--task",
            task,
            "--kind",
            "implementer",
            "--mode",
            participant.mode,
            "--harness",
            participant.harness,
            "--model",
            participant.dispatch_model,
            "--effort",
            participant.effort,
            "--brief",
            str(brief),
            "--from",
            source_ref,
            "--branch",
            branch,
            "--reason",
            reason,
        ],
        cwd=ledger,
    )
    match = STARTED_TX.search(output)
    if not match:
        raise RuntimeError(f"dispatch did not print started_tx for {task}:\n{output}")
    print(f"launched {task}: {participant.identity}", file=sys.stderr, flush=True)
    return Dispatch(task, match.group(1), participant)


def wait_barrier(ledger: Path, dispatches: list[Dispatch], timeout: str) -> None:
    args = ["orgasmic", "manager", "dispatch-wait"]
    for dispatch in dispatches:
        args.extend(["--started-tx", dispatch.started_tx])
    args.extend(["--timeout", timeout])
    for attempt in range(2):
        try:
            command(args, cwd=ledger)
            return
        except CommandError as error:
            if error.returncode != 1:
                raise
            statuses = []
            for dispatch in dispatches:
                try:
                    statuses.append(
                        command(
                            [
                                "orgasmic",
                                "manager",
                                "dispatch-status",
                                "--task",
                                dispatch.task,
                            ],
                            cwd=ledger,
                        )
                    )
                except CommandError:
                    statuses.append("status unavailable")
            if attempt == 0 and all(
                "[run-live]" in status or "[reported]" in status
                for status in statuses
            ):
                time.sleep(2)
                continue
            generations = ", ".join(
                f"{dispatch.task}={dispatch.started_tx}" for dispatch in dispatches
            )
            detail = "twice" if attempt else "and dispatch-status was unavailable"
            raise WaitUnknown(
                f"dispatch-wait lost daemon contact {detail}; worker state is unknown, "
                f"so generations were left open for recovery: {generations}"
            ) from error


def report_path(ledger: Path, dispatch: Dispatch) -> Path:
    return (
        ledger
        / f".orgasmic/tasks/{dispatch.task}/dispatches/{dispatch.started_tx}/report.md"
    )


def close_and_finish(project: str, ledger: Path, dispatch: Dispatch) -> Path:
    command(
        [
            "orgasmic",
            "manager",
            "dispatch-close",
            "--task",
            dispatch.task,
            "--started-tx",
            dispatch.started_tx,
            "--status",
            "aborted",
            "--reason",
            "successful report-only run; no source merge exists",
            "--branch-delete",
        ],
        cwd=ledger,
    )
    dispatch.closed = True
    path = report_path(ledger, dispatch)
    if not path.is_file() or not path.read_text().strip():
        raise RuntimeError(f"promoted report missing or empty: {path}")
    evidence = (
        f"- Promoted dispatch report: {dispatch.task} generation {dispatch.started_tx}\n"
        f"- Report path: {path.relative_to(ledger)}\n"
    )
    command(
        [
            "orgasmic",
            "node",
            "body",
            "set",
            "--project",
            project,
            "--kind",
            "task",
            "--section",
            "Evidence",
            "--create",
            "--body",
            evidence,
            dispatch.task,
        ],
        cwd=ledger,
    )
    finish_task(project, ledger, dispatch.task)
    return path


def finish_task(project: str, ledger: Path, task: str) -> None:
    next_state = {
        "backlog": "in_progress",
        "todo": "in_progress",
        "in_progress": "in_review",
        "in_review": "done",
    }
    while True:
        state = json.loads(
            command(
                ["orgasmic", "task", "get", "--project", project, task], cwd=ledger
            )
        )["lifecycle_stage"]
        if state == "done":
            return
        if state not in next_state:
            raise RuntimeError(f"cannot finish {task} from lifecycle state {state}")
        command(
            [
                "orgasmic",
                "task",
                "update",
                "--project",
                project,
                "--state",
                next_state[state],
                "--reason",
                "report promoted and recorded as evidence",
                task,
            ],
            cwd=ledger,
        )


def manifest_entry(label: str, participant: Participant, task: str, path: Path) -> str:
    return f"- {label}: {participant.identity}\n  Task: {task}\n  Report: {path}"


def find_submitted_artifact(ledger: Path, report: str) -> str:
    candidates = list(dict.fromkeys(ARTIFACT_ID.findall(report)))
    for artifact in reversed(candidates):
        if (ledger / f".orgasmic/artifacts/{artifact}/artifact.mdx").is_file():
            return artifact
    raise RuntimeError(f"curator report named no submitted artifact: {candidates}")


def best_effort_close(ledger: Path, dispatch: Dispatch) -> None:
    if dispatch.closed:
        return
    try:
        command(
            [
                "orgasmic",
                "manager",
                "dispatch-close",
                "--task",
                dispatch.task,
                "--started-tx",
                dispatch.started_tx,
                "--status",
                "aborted",
                "--reason",
                "multi-model orchestrator failed",
                "--branch-delete",
            ],
            cwd=ledger,
        )
        dispatch.closed = True
    except Exception as error:  # preserve the original pipeline failure
        print(f"cleanup failed for {dispatch.task}: {error}", file=sys.stderr)


def run_pipeline(args: argparse.Namespace) -> dict[str, object]:
    question = (
        Path(args.question_file).read_text() if args.question_file else args.question
    ).strip()
    if not question:
        raise ValueError("question must not be empty")
    participants = [parse_participant(raw) for raw in args.participant]
    validate_participants(participants)
    if args.curator < 1 or args.curator > len(participants):
        raise ValueError("--curator must select a 1-based participant entry")
    curator = participants[args.curator - 1]
    project, ledger, default_branch = project_context(args.project)
    started_at = datetime.now(timezone.utc).isoformat()
    source_ref = args.source_ref
    if not source_ref:
        branch = command(["git", "branch", "--show-current"]).strip()
        source_ref = (
            default_branch
            if branch == project
            else command(["git", "rev-parse", "HEAD"]).strip()
        )

    parent = command(["orgasmic", "id", "mint", "--class", "task"]).strip()
    one_line_question = " ".join(question.split())
    roster = " — ".join(p.identity for p in participants)
    create_task(
        project,
        ledger,
        parent,
        f"Multi-model extraction: {one_line_question[:100]}",
        f"Question: {one_line_question}\n\nParticipants: {roster}",
        "All extraction and blind-review reports are promoted and one curated artifact is submitted.",
        read_scope="named promoted dispatch reports",
        write_scope="orgasmic tasks and artifact store via CLI only",
    )
    command(
        [
            "orgasmic",
            "task",
            "update",
            "--project",
            project,
            "--state",
            "in_progress",
            "--reason",
            "multi-model extraction started",
            parent,
        ],
        cwd=ledger,
    )
    print(f"parent_task={parent}", file=sys.stderr, flush=True)

    active: list[Dispatch] = []
    extraction: list[tuple[Participant, Dispatch, Path]] = []
    reviews: list[tuple[Participant, Dispatch, Path]] = []
    try:
        with tempfile.TemporaryDirectory(prefix=f"orgasmic-{parent.lower()}-") as tmp_raw:
            tmp = Path(tmp_raw)
            extract_brief = tmp / "extract.md"
            extract_brief.write_text(
                compile_prompt(
                    project,
                    "extractor",
                    {"artifact.user_prompt": question},
                    ledger,
                )
            )
            extract_dispatches: list[Dispatch] = []
            for index, participant in enumerate(participants, 1):
                task = f"{parent}.{index}"
                create_task(
                    project,
                    ledger,
                    task,
                    f"Extract — {participant.identity}",
                    "Answer the parent run question independently. This is report-only; do not edit project source.",
                    "A standalone evidence-led extraction report is promoted.",
                    read_scope="question in dispatch brief; public or repository sources as needed",
                    write_scope="none; dispatch report only",
                )
                dispatch = launch(
                    ledger,
                    task,
                    participant,
                    extract_brief,
                    source_ref,
                    f"mm-{parent[5:].lower()}-extract-{index}",
                    "independent multi-model extraction",
                )
                active.append(dispatch)
                extract_dispatches.append(dispatch)
            wait_barrier(ledger, extract_dispatches, args.timeout)
            for participant, dispatch in zip(participants, extract_dispatches):
                extraction.append(
                    (participant, dispatch, close_and_finish(project, ledger, dispatch))
                )

            review_dispatches: list[Dispatch] = []
            for offset, participant in enumerate(participants, 1):
                task = f"{parent}.{len(participants) + offset}"
                others = [item for item in extraction if item[0] != participant]
                report_manifest = "\n\n".join(
                    manifest_entry("Extraction to review", p, d.task, path)
                    for p, d, path in others
                )
                review_brief = tmp / f"cross-review-{offset}.md"
                review_brief.write_text(
                    compile_prompt(
                        project,
                        "cross-reviewer",
                        {
                            "artifact.user_prompt": question,
                            "dispatch.brief": report_manifest,
                        },
                        ledger,
                    )
                )
                create_task(
                    project,
                    ledger,
                    task,
                    f"Blind cross-review — {participant.identity}",
                    "Review only the other participants' promoted extraction reports. This is a fresh report-only dispatch.",
                    "A ? / + / = delta report is promoted without access to this participant's own extraction.",
                    read_scope="other participants' report paths named in dispatch brief",
                    write_scope="none; dispatch report only",
                )
                dispatch = launch(
                    ledger,
                    task,
                    participant,
                    review_brief,
                    source_ref,
                    f"mm-{parent[5:].lower()}-review-{offset}",
                    "blind cross-review of other model reports",
                )
                active.append(dispatch)
                review_dispatches.append(dispatch)
            wait_barrier(ledger, review_dispatches, args.timeout)
            for participant, dispatch in zip(participants, review_dispatches):
                reviews.append(
                    (participant, dispatch, close_and_finish(project, ledger, dispatch))
                )

            curator_task = f"{parent}.{2 * len(participants) + 1}"
            run_manifest = (
                f"Parent task: {parent}\n"
                f"Started UTC: {started_at}\n"
                f"Participants ({len(participants)}):\n"
                + "\n".join(f"- {p.identity}" for p in participants)
                + f"\nCurator: {curator.identity}\n\n"
                + "\n\n".join(
                    manifest_entry("Extraction", p, d.task, path)
                    for p, d, path in extraction
                )
                + "\n\n"
                + "\n\n".join(
                    manifest_entry("Cross-review", p, d.task, path)
                    for p, d, path in reviews
                )
                + f"\n\nCuration task: {curator_task}"
            )
            curator_brief = tmp / "curator.md"
            curator_brief.write_text(
                compile_prompt(
                    project,
                    "curator",
                    {
                        "artifact.user_prompt": question,
                        "dispatch.brief": run_manifest,
                        "task.id": curator_task,
                    },
                    ledger,
                )
            )
            create_task(
                project,
                ledger,
                curator_task,
                f"Curate artifact — {curator.identity}",
                "Read all promoted extraction and cross-review reports, submit one final MDX artifact, and report its id.",
                "The submitted artifact matches the multi-model final-artifact contract and names every raw-report task.",
                read_scope="all promoted report paths named in dispatch brief and MDX block contract",
                write_scope="artifact store via orgasmic artifact submit only",
            )
            curator_dispatch = launch(
                ledger,
                curator_task,
                curator,
                curator_brief,
                source_ref,
                f"mm-{parent[5:].lower()}-curate",
                "curate multi-model reports into final artifact",
            )
            active.append(curator_dispatch)
            wait_barrier(ledger, [curator_dispatch], args.timeout)
            curator_report_path = close_and_finish(project, ledger, curator_dispatch)
            artifact = find_submitted_artifact(ledger, curator_report_path.read_text())

        parent_evidence = (
            f"- Artifact: {artifact}\n"
            f"- Extraction tasks: {' '.join(d.task for _, d, _ in extraction)}\n"
            f"- Cross-review tasks: {' '.join(d.task for _, d, _ in reviews)}\n"
            f"- Curation task: {curator_task}\n"
        )
        command(
            [
                "orgasmic",
                "node",
                "body",
                "set",
                "--project",
                project,
                "--kind",
                "task",
                "--section",
                "Evidence",
                "--create",
                "--body",
                parent_evidence,
                parent,
            ],
            cwd=ledger,
        )
        finish_task(project, ledger, parent)
        return {
            "parent_task": parent,
            "extraction_tasks": [dispatch.task for _, dispatch, _ in extraction],
            "cross_review_tasks": [dispatch.task for _, dispatch, _ in reviews],
            "curation_task": curator_task,
            "artifact_id": artifact,
        }
    except Exception as error:
        if not isinstance(error, WaitUnknown):
            for dispatch in active:
                best_effort_close(ledger, dispatch)
        raise


def self_test() -> None:
    codex = parse_participant("stdio,codex,gpt-5.6-luna,low")
    hermes = parse_participant("stdio,hermes,google/gemini-3.7-flash,medium")
    assert codex.identity == "codex · openai · gpt-5.6-luna · effort low"
    assert hermes.vendor == "google" and hermes.model == "gemini-3.7-flash"
    assert STARTED_TX.search("run_id=r started_tx=tx-123 worker=w").group(1) == "tx-123"
    assert ARTIFACT_ID.findall("submitted ART-9Z8YX") == ["ART-9Z8YX"]
    try:
        validate_participants([codex, codex])
    except ValueError:
        pass
    else:
        raise AssertionError("duplicate models must be rejected")
    print("self-test ok")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    question = result.add_mutually_exclusive_group(required=False)
    question.add_argument("--question")
    question.add_argument("--question-file")
    result.add_argument("--participant", action="append", default=[])
    result.add_argument("--curator", type=int, default=1)
    result.add_argument("--project")
    result.add_argument("--from", dest="source_ref")
    result.add_argument("--timeout", default="45m")
    result.add_argument("--self-test", action="store_true")
    return result


def main() -> None:
    args = parser().parse_args()
    if args.self_test:
        self_test()
        return
    if not (args.question or args.question_file):
        raise SystemExit("one of --question or --question-file is required")
    if not args.participant:
        raise SystemExit("repeat --participant at least twice")
    print(json.dumps(run_pipeline(args), indent=2))


if __name__ == "__main__":
    main()
