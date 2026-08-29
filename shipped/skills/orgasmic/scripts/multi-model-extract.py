#!/usr/bin/env python3
"""Compose existing orgasmic verbs into a multi-model knowledge run."""

from __future__ import annotations

import argparse
import base64
import html
import json
import re
import shlex
import subprocess
import sys
import tempfile
import textwrap
import time
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


ARTIFACT_ID = re.compile(r"\bART-[0-9A-HJKMNP-TV-Z]{5}\b")
STARTED_TX = re.compile(r"\bstarted_tx=([^\s]+)")
QUESTION_PLACEHOLDER = "__ORGASMIC_QUESTION_SECTION__"
DIAGRAM_PLACEHOLDER = "__ORGASMIC_PIPELINE_DIAGRAM__"
SVG_AUTHORED_BY_MODEL = re.compile(r"<\s*svg\b|data:image/svg\+xml", re.IGNORECASE)
VENDOR_COLORS = {
    "anthropic": "#d97757",
    "openai": "#10a37f",
    "google": "#6f9df2",
}


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


def clipped(value: str, limit: int) -> str:
    value = " ".join(value.split())
    return value if len(value) <= limit else value[: limit - 1].rstrip() + "…"


def svg_text(
    x: float,
    y: float,
    value: str,
    style: str,
    attrs: dict[str, str] | None = None,
) -> str:
    extra = "".join(
        f' {name}="{html.escape(raw, quote=True)}"'
        for name, raw in (attrs or {}).items()
    )
    return (
        f'<text x="{x:g}" y="{y:g}" style="{html.escape(style, quote=True)}"'
        f"{extra}>{html.escape(value)}</text>"
    )


def load_diagram_fields(
    path: Path, extraction_tasks: list[str], review_tasks: list[str]
) -> tuple[dict[str, list[str]], dict[str, list[dict[str, str]]], str]:
    raw = path.read_text()
    if SVG_AUTHORED_BY_MODEL.search(raw):
        raise RuntimeError("curator diagram fields contained model-authored SVG")
    data = json.loads(raw)
    if not isinstance(data, dict):
        raise ValueError("curator diagram fields must be a JSON object")

    extracts: dict[str, list[str]] = {}
    for item in data.get("extracts", []):
        if not isinstance(item, dict):
            raise ValueError("each extract diagram entry must be an object")
        task, lines = item.get("task"), item.get("excerpt_lines")
        if (
            task in extracts
            or task not in extraction_tasks
            or not isinstance(lines, list)
            or not 1 <= len(lines) <= 4
            or not all(isinstance(line, str) and line.strip() for line in lines)
        ):
            raise ValueError(f"invalid extract diagram entry for {task!r}")
        extracts[task] = [clipped(line, 43) for line in lines]
    if set(extracts) != set(extraction_tasks):
        raise ValueError("curator diagram fields must cover every extraction task once")

    reviews: dict[str, list[dict[str, str]]] = {}
    for item in data.get("reviews", []):
        if not isinstance(item, dict):
            raise ValueError("each review diagram entry must be an object")
        task, bullets = item.get("task"), item.get("delta_bullets")
        valid_bullets = (
            isinstance(bullets, list)
            and len(bullets) == 3
            and all(
                isinstance(bullet, dict)
                and bullet.get("tag") in {"?", "+", "="}
                and isinstance(bullet.get("text"), str)
                and bullet["text"].strip()
                for bullet in bullets
            )
        )
        if (
            task in reviews
            or task not in review_tasks
            or not valid_bullets
            or {bullet["tag"] for bullet in bullets} != {"?", "+", "="}
        ):
            raise ValueError(f"invalid review diagram entry for {task!r}")
        reviews[task] = [
            {"tag": bullet["tag"], "text": clipped(bullet["text"], 43)}
            for bullet in bullets
        ]
    if set(reviews) != set(review_tasks):
        raise ValueError("curator diagram fields must cover every review task once")

    summary = data.get("curator_summary")
    if not isinstance(summary, str) or not summary.strip():
        raise ValueError("curator_summary must be a non-empty string")
    return extracts, reviews, clipped(summary, 72)


def render_pipeline_svg(
    question: str,
    extraction: list[tuple[Participant, Dispatch, Path]],
    reviews: list[tuple[Participant, Dispatch, Path]],
    curator: Participant,
    curator_task: str,
    curator_path: Path,
    extract_lines: dict[str, list[str]],
    review_bullets: dict[str, list[dict[str, str]]],
    curator_summary: str,
) -> str:
    if len(extraction) < 2 or len(extraction) != len(reviews):
        raise ValueError("diagram requires matching extraction and review rosters")
    participants = [participant for participant, _, _ in extraction]
    if participants != [participant for participant, _, _ in reviews]:
        raise ValueError("diagram extraction and review roster order must match")

    count = len(participants)
    card_width, gap, margin, height = 252, 30, 32, 1000
    width = margin * 2 + count * card_width + (count - 1) * gap
    center = width / 2
    card_xs = [margin + index * (card_width + gap) for index in range(count)]
    card_centers = [x + card_width / 2 for x in card_xs]
    prompt_width = min(480, width - 64)
    prompt_x = center - prompt_width / 2
    curator_x = center - 200
    vendor_color = lambda vendor: VENDOR_COLORS.get(vendor.lower(), "#b9a998")

    sans = (
        "font-family:-apple-system,'SF Pro Text','Segoe UI',Helvetica,Arial,sans-serif"
    )
    mono = "font-family:ui-monospace,'SF Mono',Menlo,Consolas,monospace"
    stage_style = (
        f"{mono};font-size:8px;font-weight:500;fill:#8f7f70;"
        "text-anchor:middle;letter-spacing:0.14em"
    )
    vendor_style = (
        f"{mono};font-size:8.5px;font-weight:500;fill:#b9a998;"
        "text-anchor:start;letter-spacing:0.12em"
    )
    model_style = (
        f"{sans};font-size:15px;font-weight:700;fill:#f0e6da;text-anchor:start"
    )
    role_style = f"{mono};font-size:8px;font-weight:400;fill:#8f7f70;text-anchor:start"
    body_style = (
        f"{sans};font-size:10.5px;font-weight:400;fill:#b9a998;text-anchor:start"
    )
    path_style = f"{mono};font-size:8px;font-weight:400;fill:#8f7f70;text-anchor:start"
    border = "rgba(240,230,218,0.13)"

    question_lines = textwrap.wrap(
        " ".join(question.split()),
        width=68,
        break_long_words=True,
        break_on_hyphens=False,
    ) or [""]
    if len(question_lines) > 2:
        question_lines = [question_lines[0], clipped(" ".join(question_lines[1:]), 68)]
    question_lines += [""] * (2 - len(question_lines))

    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" '
        'aria-label="Forward pipeline from prompt to final answer">',
        f'<rect x="0.5" y="0.5" width="{width - 1}" height="999" rx="16" '
        f'fill="#241a15" stroke="{border}"/>',
        '<defs><marker id="ah" markerWidth="7" markerHeight="7" refX="6" refY="3.5" '
        'orient="auto"><path d="M0,0 L7,3.5 L0,7 z" fill="#b9a998"/></marker></defs>',
        '<g data-card="prompt">',
        f'<rect x="{prompt_x:g}" y="36" width="{prompt_width:g}" height="92" rx="10" '
        f'fill="#2c211b" stroke="{border}" stroke-width="1"/>',
        svg_text(
            center,
            60,
            "PROMPT",
            f"{mono};font-size:8px;font-weight:500;fill:#8f7f70;"
            "text-anchor:middle;letter-spacing:0.18em",
        ),
        svg_text(
            center,
            82,
            question_lines[0],
            f"{sans};font-size:12px;font-weight:500;fill:#f0e6da;text-anchor:middle",
        ),
        svg_text(
            center,
            100,
            question_lines[1],
            f"{sans};font-size:12px;font-weight:500;fill:#f0e6da;text-anchor:middle",
        ),
        "</g>",
    ]

    for destination in card_centers:
        out.append(
            f'<path d="M{center:g},128 C{center:g},164 {destination:g},164 {destination:g},200" '
            'fill="none" stroke="#b9a998" stroke-width="1.25" opacity="0.55" '
            'marker-end="url(#ah)"/>'
        )
    out.extend(
        [
            '<g data-pill="extract">',
            f'<rect x="{center - 130:g}" y="153" width="260" height="22" rx="11" '
            f'fill="#241a15" stroke="{border}"/>',
            svg_text(center, 167, "1 · EXTRACT — PARALLEL · ISOLATED", stage_style),
            "</g>",
        ]
    )

    for x, (participant, dispatch, path) in zip(card_xs, extraction):
        lines = [clipped(line, 43) for line in extract_lines[dispatch.task][:4]]
        lines += [""] * (4 - len(lines))
        short_path = f"{dispatch.task}/…/report.md"
        out.extend(
            [
                f'<g data-card="extract" data-task="{html.escape(dispatch.task, quote=True)}" '
                f'data-record-path="{html.escape(str(path), quote=True)}">',
                f'<rect x="{x}" y="200" width="252" height="224" rx="10" '
                f'fill="#2c211b" stroke="{border}" stroke-width="1"/>',
                f'<circle cx="{x + 20}" cy="226" r="4" fill="{vendor_color(participant.vendor)}"/>',
                svg_text(x + 32, 229, participant.vendor.upper(), vendor_style),
                svg_text(x + 18, 256, clipped(participant.model, 32), model_style),
                svg_text(
                    x + 18,
                    272,
                    clipped(
                        f"{participant.harness} · extract · effort {participant.effort} · {dispatch.task}",
                        55,
                    ),
                    role_style,
                ),
                f'<line x1="{x + 18}" y1="284" x2="{x + 234}" y2="284" stroke="{border}"/>',
                *[
                    svg_text(x + 18, 305 + line_index * 17, line, body_style)
                    for line_index, line in enumerate(lines)
                ],
                svg_text(x + 18, 406, short_path, path_style),
                "</g>",
            ]
        )

    for source_index, source in enumerate(card_centers):
        for target_index, destination in enumerate(card_centers):
            if source_index == target_index:
                continue
            out.append(
                f'<path d="M{source:g},424 C{source:g},464 {destination:g},464 {destination:g},504" '
                'fill="none" stroke="#b9a998" stroke-width="1.25" opacity="0.55" '
                'marker-end="url(#ah)"/>'
            )
    out.extend(
        [
            '<g data-pill="cross-review">',
            f'<rect x="{center - 140:g}" y="453" width="280" height="22" rx="11" '
            f'fill="#241a15" stroke="{border}"/>',
            svg_text(center, 467, "2 · CROSS-REVIEW — BLIND · NEVER SELF", stage_style),
            "</g>",
        ]
    )

    for index, (x, (participant, dispatch, path)) in enumerate(zip(card_xs, reviews)):
        read_models = " + ".join(
            p.model for offset, p in enumerate(participants) if offset != index
        )
        out.extend(
            [
                f'<g data-card="review" data-task="{html.escape(dispatch.task, quote=True)}" '
                f'data-record-path="{html.escape(str(path), quote=True)}">',
                f'<rect x="{x}" y="504" width="252" height="200" rx="10" '
                f'fill="#2c211b" stroke="{border}" stroke-width="1"/>',
                f'<circle cx="{x + 20}" cy="530" r="4" fill="{vendor_color(participant.vendor)}"/>',
                svg_text(x + 32, 533, participant.vendor.upper(), vendor_style),
                svg_text(x + 18, 558, clipped(participant.model, 32), model_style),
                svg_text(
                    x + 18, 574, clipped(f"read {read_models} · blind", 42), role_style
                ),
                f'<line x1="{x + 18}" y1="586" x2="{x + 234}" y2="586" stroke="{border}"/>',
            ]
        )
        for bullet_index, bullet in enumerate(review_bullets[dispatch.task]):
            glyph = bullet["tag"]
            glyph_fill = (
                "#f08a59" if glyph == "?" else "#f0e6da" if glyph == "+" else "#8f7f70"
            )
            y = 608 + bullet_index * 20
            out.append(
                svg_text(
                    x + 18,
                    y,
                    glyph,
                    f"{mono};font-size:10.5px;font-weight:700;fill:{glyph_fill};text-anchor:start",
                    {"data-delta": glyph},
                )
            )
            out.append(svg_text(x + 32, y, clipped(bullet["text"], 43), body_style))
        out.extend(
            [
                svg_text(x + 18, 686, f"{dispatch.task}/…/report.md", path_style),
                "</g>",
            ]
        )

    for source in card_centers:
        out.append(
            f'<path d="M{source:g},704 C{source:g},740 {center:g},740 {center:g},776" '
            'fill="none" stroke="#b9a998" stroke-width="1.25" opacity="0.55" '
            'marker-end="url(#ah)"/>'
        )
    out.extend(
        [
            '<g data-pill="curate">',
            f'<rect x="{center - 60:g}" y="729" width="120" height="22" rx="11" '
            f'fill="#241a15" stroke="{border}"/>',
            svg_text(center, 743, "3 · CURATE", stage_style),
            "</g>",
            f'<g data-card="curator" data-task="{html.escape(curator_task, quote=True)}" '
            f'data-record-path="{html.escape(str(curator_path), quote=True)}">',
            f'<rect x="{curator_x:g}" y="776" width="400" height="92" rx="10" '
            'fill="rgba(240,138,89,0.10)" stroke="#f08a59" stroke-width="1.5"/>',
            f'<circle cx="{curator_x + 22:g}" cy="802" r="4" fill="{vendor_color(curator.vendor)}"/>',
            svg_text(curator_x + 34, 805, curator.vendor.upper(), vendor_style),
            svg_text(
                curator_x + 18,
                830,
                clipped(f"{curator.model} · curator", 48),
                f"{sans};font-size:14px;font-weight:700;fill:#f0e6da;text-anchor:start",
            ),
            svg_text(
                curator_x + 18,
                848,
                curator_summary,
                f"{mono};font-size:8.5px;font-weight:400;fill:#b9a998;text-anchor:start",
            ),
            svg_text(curator_x + 18, 862, f"{curator_task}/…/report.md", path_style),
            "</g>",
            f'<line x1="{center:g}" y1="868" x2="{center:g}" y2="906" stroke="#b9a998" '
            'stroke-width="1.25" opacity="0.55" marker-end="url(#ah)"/>',
            '<g data-pill="final-answer">',
            f'<rect x="{center - 190:g}" y="912" width="380" height="54" rx="27" fill="#f08a59"/>',
            svg_text(
                center,
                936,
                "FINAL ANSWER",
                f"{sans};font-size:13px;font-weight:800;fill:#241a15;"
                "text-anchor:middle;letter-spacing:0.06em",
            ),
            svg_text(
                center,
                952,
                "at the top of this page",
                f"{mono};font-size:9.5px;font-weight:500;fill:#241a15;"
                "text-anchor:middle;opacity:0.75",
            ),
            "</g>",
            "</svg>",
        ]
    )
    return "".join(out)


def assemble_artifact(draft: str, question: str, svg: str, raw_tasks: list[str]) -> str:
    if SVG_AUTHORED_BY_MODEL.search(draft):
        raise RuntimeError("curator draft contained model-authored SVG")
    if draft.count(QUESTION_PLACEHOLDER) != 1 or draft.count(DIAGRAM_PLACEHOLDER) != 1:
        raise RuntimeError(
            "curator draft must contain each orchestrator placeholder once"
        )

    escaped_question = (
        html.escape(question, quote=False).replace("{", "&#123;").replace("}", "&#125;")
    )
    question_section = (
        '<Section title="Question">\n<RichText>\n'
        f"{escaped_question}\n"
        "</RichText>\n</Section>"
    )
    image = (
        '<Image src="data:image/svg+xml;base64,'
        + base64.b64encode(svg.encode()).decode()
        + '" alt="Question flows through independent extraction and blind cross-review into curation" '
        'caption="From the verbatim question to the curated final answer." />'
    )
    mdx = draft.replace(QUESTION_PLACEHOLDER, question_section).replace(
        DIAGRAM_PLACEHOLDER, image
    )

    sections = re.findall(r'<Section(?:\s+title="([^"]*)")?[^>]*>', mdx)
    if not sections or sections[0] != "Question":
        raise RuntimeError("Question must be the first Section")
    match = re.search(
        r'<Section\s+title="Question"[^>]*>\s*<RichText>\s*(.*?)\s*</RichText>\s*</Section>',
        mdx,
        re.DOTALL,
    )
    if not match or html.unescape(match.group(1)) != question:
        raise RuntimeError(
            "Question section does not match the input question verbatim"
        )
    required = {"Question", "Final answer", "From question to answer", "Knowledge map"}
    if not required.issubset(set(sections)):
        raise RuntimeError(
            f"curator draft is missing required sections: {sorted(required - set(sections))}"
        )
    missing_tasks = [
        task
        for task in raw_tasks
        if not re.search(rf"{re.escape(task)}(?![.\d])", mdx)
    ]
    if missing_tasks:
        raise RuntimeError(
            f"curator draft omitted raw-report task ids: {missing_tasks}"
        )
    return mdx


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
    if any(
        placeholder in question
        for placeholder in (QUESTION_PLACEHOLDER, DIAGRAM_PLACEHOLDER)
    ):
        raise ValueError("question must not contain orchestrator placeholders")
    if question.startswith("-"):
        raise ValueError("question must not start with '-'")
    participants = [parse_participant(raw) for raw in args.participant]
    validate_participants(participants)
    if args.curator < 1 or args.curator > len(participants):
        raise ValueError("--curator must select a 1-based participant entry")
    if args.artifact_id and not ARTIFACT_ID.fullmatch(args.artifact_id):
        raise ValueError(
            "--artifact-id must be ART- followed by five Crockford characters"
        )
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
                f"Curate answer — {curator.identity}",
                "Read all promoted extraction and cross-review reports, write the final prose draft and structured diagram fields, and report their paths.",
                "The prose draft matches the final-artifact contract, names every raw-report task, and contains only orchestrator placeholders for the Question and diagram.",
                read_scope="all promoted report paths named in dispatch brief and MDX block contract",
                write_scope="/tmp curation draft, diagram JSON, and dispatch report only",
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
            draft_path = Path(f"/tmp/{curator_task}-curation.mdx")
            fields_path = Path(f"/tmp/{curator_task}-diagram.json")
            if not draft_path.is_file() or not fields_path.is_file():
                raise RuntimeError(
                    f"curator outputs missing: draft={draft_path.is_file()} fields={fields_path.is_file()}"
                )
            extract_lines, review_bullets, curator_summary = load_diagram_fields(
                fields_path,
                [dispatch.task for _, dispatch, _ in extraction],
                [dispatch.task for _, dispatch, _ in reviews],
            )
            svg = render_pipeline_svg(
                question,
                extraction,
                reviews,
                curator,
                curator_task,
                curator_report_path,
                extract_lines,
                review_bullets,
                curator_summary,
            )
            raw_tasks = [dispatch.task for _, dispatch, _ in extraction + reviews] + [
                curator_task
            ]
            mdx = assemble_artifact(draft_path.read_text(), question, svg, raw_tasks)
            artifact = (
                args.artifact_id
                or command(
                    ["orgasmic", "id", "mint", "--class", "artifact"], cwd=ledger
                ).strip()
            )
            assembled_path = tmp / f"{artifact}.mdx"
            assembled_path.write_text(mdx)
            submission = command(
                [
                    "orgasmic",
                    "artifact",
                    "submit",
                    artifact,
                    "--project",
                    project,
                    "--file",
                    str(assembled_path),
                    "--title",
                    f"Multi-model extraction: {one_line_question[:100]}",
                    "--subject-nodes",
                    parent,
                    "--prompt",
                    question,
                ],
                cwd=ledger,
            ).strip()
            print(submission, file=sys.stderr, flush=True)
            draft_path.unlink()
            fields_path.unlink()

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
    claude = parse_participant("stdio,claude,claude-haiku-4-5-20251001,low")
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

    text_limit = 43
    over_limit = "x" * (text_limit + 1)
    extract_at_limit = "e" * text_limit
    review_at_limit = "r" * text_limit
    with tempfile.TemporaryDirectory() as raw:
        fields_path = Path(raw) / "diagram.json"
        fields_path.write_text(
            json.dumps(
                {
                    "extracts": [{"task": "TASK-CAP.1", "excerpt_lines": [over_limit]}],
                    "reviews": [
                        {
                            "task": "TASK-CAP.2",
                            "delta_bullets": [
                                {"tag": tag, "text": over_limit} for tag in "?+="
                            ],
                        }
                    ],
                    "curator_summary": "summary",
                }
            )
        )
        cap_extracts, cap_reviews, _ = load_diagram_fields(
            fields_path, ["TASK-CAP.1"], ["TASK-CAP.2"]
        )
    expected_cap = "x" * (text_limit - 1) + "…"
    assert cap_extracts["TASK-CAP.1"] == [expected_cap]
    assert cap_reviews["TASK-CAP.2"][0]["text"] == expected_cap

    rejected_questions = {
        f"contains {QUESTION_PLACEHOLDER}": "orchestrator placeholders",
        f"contains {DIAGRAM_PLACEHOLDER}": "orchestrator placeholders",
        "-leading option-shaped question": "must not start",
    }
    for rejected, message in rejected_questions.items():
        try:
            run_pipeline(argparse.Namespace(question=rejected, question_file=None))
        except ValueError as error:
            assert message in str(error)
        else:
            raise AssertionError(f"question must be rejected up front: {rejected}")

    for count in (2, 3):
        participants = [codex, hermes, claude][:count]
        extraction = [
            (
                participant,
                Dispatch(f"TASK-TESTX.{index}", f"tx-extract-{index}", participant),
                Path(
                    f"/ledger/.orgasmic/tasks/TASK-TESTX.{index}/dispatches/tx/report.md"
                ),
            )
            for index, participant in enumerate(participants, 1)
        ]
        reviews = [
            (
                participant,
                Dispatch(
                    f"TASK-TESTX.{count + index}", f"tx-review-{index}", participant
                ),
                Path(
                    f"/ledger/.orgasmic/tasks/TASK-TESTX.{count + index}/dispatches/tx/report.md"
                ),
            )
            for index, participant in enumerate(participants, 1)
        ]
        extract_lines = {
            dispatch.task: [extract_at_limit, "Second short finding"]
            for _, dispatch, _ in extraction
        }
        review_bullets = {
            dispatch.task: [
                {"tag": "?", "text": review_at_limit},
                {"tag": "+", "text": "new evidence"},
                {"tag": "=", "text": "shared conclusion"},
            ]
            for _, dispatch, _ in reviews
        }
        curator_task = f"TASK-TESTX.{2 * count + 1}"
        svg = render_pipeline_svg(
            "When should append-only events be authoritative?",
            extraction,
            reviews,
            codex,
            curator_task,
            Path(f"/ledger/.orgasmic/tasks/{curator_task}/dispatches/tx/report.md"),
            extract_lines,
            review_bullets,
            "reports deduplicated; disagreements remain explicit",
        )
        assert "<style" not in svg
        assert all(
            VENDOR_COLORS[participant.vendor] in svg for participant in participants
        )
        root = ET.fromstring(svg)
        width = int(root.attrib["width"])
        height = int(root.attrib["height"])
        assert root.attrib["viewBox"] == f"0 0 {width} {height}"
        assert width == 64 + count * 252 + (count - 1) * 30 and height == 1000
        namespace = "{http://www.w3.org/2000/svg}"
        groups = root.findall(f".//{namespace}g")
        cards = [group for group in groups if group.get("data-card")]
        pills = [group for group in groups if group.get("data-pill")]
        texts = root.findall(f".//{namespace}text")
        assert {extract_at_limit, review_at_limit}.issubset(
            {node.text for node in texts}
        )
        assert len(cards) == 2 * count + 2
        assert len(pills) == 4
        assert len(texts) == 12 + 18 * count
        assert {
            "1 · EXTRACT — PARALLEL · ISOLATED",
            "2 · CROSS-REVIEW — BLIND · NEVER SELF",
            "3 · CURATE",
            "FINAL ANSWER",
        }.issubset({node.text for node in texts})
        for glyph in "?+=":
            assert sum(node.get("data-delta") == glyph for node in texts) == count
        for node in texts:
            assert "style" in node.attrib
            assert not {
                "font-family",
                "font-size",
                "font-weight",
                "fill",
                "text-anchor",
                "letter-spacing",
            }.intersection(node.attrib)

        raw_tasks = [dispatch.task for _, dispatch, _ in extraction + reviews] + [
            curator_task
        ]
        draft = (
            "<RichText>Run header</RichText>\n"
            '<Callout tone="warning">Verify claims.</Callout>\n'
            f"{QUESTION_PLACEHOLDER}\n"
            '<Section title="Final answer"><RichText>Answer.</RichText></Section>\n'
            '<Section title="From question to answer">\n'
            f"{DIAGRAM_PLACEHOLDER}\n"
            f"<RichText>Raw reports: {' '.join(raw_tasks)}</RichText>\n</Section>\n"
            '<Section title="Knowledge map"><RichText>Map.</RichText></Section>\n'
            "<Section><RichText>Feedback.</RichText></Section>"
        )
        question = "Should <svg> and {braces} stay verbatim & safe?"
        assembled = assemble_artifact(draft, question, svg, raw_tasks)
        assert assembled.index('title="Question"') < assembled.index(
            'title="Final answer"'
        )
        assert assembled.count("data:image/svg+xml;base64,") == 1
        try:
            assemble_artifact(
                draft.replace(DIAGRAM_PLACEHOLDER, "<svg/>"), question, svg, raw_tasks
            )
        except RuntimeError as error:
            assert "model-authored SVG" in str(error)
        else:
            raise AssertionError("model-authored SVG must be rejected")
        try:
            assemble_artifact(
                draft.replace(raw_tasks[0], f"{raw_tasks[0]}1"),
                question,
                svg,
                raw_tasks,
            )
        except RuntimeError as error:
            assert "omitted raw-report task ids" in str(error)
        else:
            raise AssertionError(f"{raw_tasks[0]}1 must not satisfy {raw_tasks[0]}")
    print("self-test ok")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    question = result.add_mutually_exclusive_group(required=False)
    question.add_argument("--question")
    question.add_argument("--question-file")
    result.add_argument("--participant", action="append", default=[])
    result.add_argument("--curator", type=int, default=1)
    result.add_argument("--artifact-id", help="resubmit an existing artifact id")
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
