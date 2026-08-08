#!/usr/bin/env python3
# orgasmic:TASK-2QK4P.1.1.1.1
"""Derive the authority-relevant observation surface instead of asserting it.

Rounds three and four of TASK-2QK4P.1.1.1 each enumerated a boundary the author
had drawn by hand, and both boundaries were internally correct and externally
incomplete: `ClaimDirectory::open`, `::read_regular` and `::names` sat one call
outside round four's hand-written path definition and were never classified.
A hand-drawn boundary answers "nothing else is on the path" when it means "I did
not look further", which is the same collapse the whole task exists to close.

So this script computes the boundary from the source instead.

    METHOD
    ------
    1. Index every `fn NAME` definition in the workspace crates, keeping file,
       line, the enclosing `impl` type when there is one, and the body text
       (brace-matched, string/char/line-comment aware).
    2. Build a call graph by scanning each body for the three call shapes Rust
       actually writes, and resolve each by CALL KIND:
         * `name(`        -> free functions only (owner is None)
         * `Type::name(`  -> methods/associated fns of exactly `Type`
         * `.name(`       -> methods of any type WHOSE TYPE NAME is mentioned in
                            the calling file (its `use`s, bindings or paths)
       The last rule is the only inexact one; it exists because a receiver's
       type is not recoverable without type inference. It OVER-approximates
       (it can add edges that do not exist) and does not under-approximate for
       calls whose receiver type is named anywhere in the file — which is the
       safe direction for a boundary derivation.
    3. BFS from the three API entry points named in the acceptance
       (`post_run_recover`, `get_recovery_inventory`, `reattach_live_runs_on_boot`)
       to get the reachable set.
    4. Report every reachable definition whose OWN body contains a
       syscall-touching token (raw `libc::`, `std::fs`, `OpenOptions`, directory
       iteration, `metadata`, `canonicalize`, `try_exists`/`exists`, `Command`,
       …) — the leaves — together with the classification-relevant shape of its
       signature (`-> bool`, `-> Option`, `Result<Option<_>>`, `Result<Vec<_>>`,
       raw `libc` loop, …).

    RESIDUAL (stated, not hidden)
    -----------------------------
    - Name-keyed edges cannot resolve trait-object / `dyn` dispatch, function
      values stored in structs, or calls constructed inside macros. Those are
      invisible to this pass; see `--report-dynamic` for the places the sources
      use `dyn`/`Box<fn>` so a reader can check them by hand.
    - Over-approximation means the reachable set is a superset. That is stated
      rather than trimmed: a boundary that is too big is reviewable, one that is
      too small is what produced rounds three, four and five.
    - `#[cfg(...)]` is not evaluated, so non-unix bodies are included.

Usage:
    python3 scripts/derive-observation-surface.py            # the leaf surface
    python3 scripts/derive-observation-surface.py --all      # every reachable fn
    python3 scripts/derive-observation-surface.py --json     # machine-readable
    python3 scripts/derive-observation-surface.py --report-dynamic
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

ENTRY_POINTS = [
    "post_run_recover",            # POST /runs/:id/recover
    "get_recovery_inventory",      # GET /runs
    "reattach_live_runs_on_boot",  # boot reattach routing
]

# Tokens that mean "this body itself talks to the filesystem / OS".
SYSCALL_TOKENS = [
    "libc::",
    "std::fs::",
    "fs::read",
    "OpenOptions",
    "File::open",
    "File::create",
    "read_dir",
    "ReadDir",
    ".metadata(",
    "canonicalize",
    "try_exists",
    ".exists(",
    "read_to_string",
    "read_to_end",
    "remove_file",
    "remove_dir",
    "create_dir",
    "sync_all",
    "sync_data",
    "Command::new",
    "symlink_metadata",
    "set_permissions",
    "hard_link",
    "std::fs::rename",
]

FN_RE = re.compile(
    r"^(?P<indent>[ \t]*)(?:pub(?:\([^)]*\))?\s+)?"
    r"(?:default\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?"
    r"(?:extern\s+\"[^\"]*\"\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)",
    re.M,
)
IMPL_RE = re.compile(
    r"^[ \t]*impl(?:<[^>]*>)?\s+(?:(?P<trait>[\w:<>, ']+)\s+for\s+)?(?P<ty>[\w]+)",
    re.M,
)


def strip_noise(text: str) -> str:
    """Blank out string/char literals and line comments so token scans do not
    match documentation prose or embedded snippets."""
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == "/" and i + 1 < n and text[i + 1] == "/":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
        elif c == "/" and i + 1 < n and text[i + 1] == "*":
            j = text.find("*/", i + 2)
            j = n if j < 0 else j + 2
            out.append(" " * (j - i))
            i = j
        elif c == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    j += 1
                    break
                j += 1
            out.append('"' + " " * (j - i - 2) + '"' if j - i >= 2 else " " * (j - i))
            i = j
        elif c == "'":
            # Char/byte literal vs lifetime. `'\\n'` and `'\"'` must be blanked
            # or a quote inside a char literal opens a phantom string and
            # swallows the rest of the function (this really happened:
            # `byte == b'\"'` in session.rs hid a whole module).
            if i + 1 < n and text[i + 1] == "\\":
                j = text.find("'", i + 2)
                j = n if j < 0 else j + 1
            elif i + 2 < n and text[i + 2] == "'":
                j = i + 3
            else:
                out.append(c)
                i += 1
                continue
            out.append(" " * (j - i))
            i = j
        else:
            out.append(c)
            i += 1
    return "".join(out)


def rust_files() -> list[str]:
    found = []
    for base in ("crates",):
        for dirpath, dirnames, filenames in os.walk(os.path.join(ROOT, base)):
            dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
            for name in filenames:
                if name.endswith(".rs"):
                    found.append(os.path.join(dirpath, name))
    return sorted(found)


def body_of(clean: str, start: int) -> tuple[int, int]:
    """Return (open_brace, close_brace) of the block that follows `start`."""
    depth = 0
    i = clean.find("{", start)
    if i < 0:
        return (-1, -1)
    open_at = i
    while i < len(clean):
        if clean[i] == "{":
            depth += 1
        elif clean[i] == "}":
            depth -= 1
            if depth == 0:
                return (open_at, i)
        i += 1
    return (open_at, len(clean) - 1)


def index_defs() -> dict:
    defs = {}
    for path in rust_files():
        raw = open(path, encoding="utf-8", errors="replace").read()
        clean = strip_noise(raw)
        # Brace-matched impl block ranges, so a free function that merely
        # follows an impl block is not attributed to its type.
        test_blocks = []
        for tm in re.finditer(r"^[ \t]*(?:pub\s+)?mod\s+tests?\b", clean, re.M):
            o, c = body_of(clean, tm.end())
            if o >= 0:
                test_blocks.append((o, c))
        impls = []
        for im in IMPL_RE.finditer(clean):
            o, c = body_of(clean, im.end())
            if o >= 0:
                impls.append((o, c, im.group("ty")))
        for m in FN_RE.finditer(clean):
            name = m.group("name")
            open_at, close_at = body_of(clean, m.end())
            if open_at < 0:
                continue
            sig = clean[m.start(): open_at]
            body = clean[open_at: close_at + 1]
            owner = None
            best = None
            for o, c, ty in impls:
                if o < m.start() < c and (best is None or o > best):
                    best, owner = o, ty
            rel = os.path.relpath(path, ROOT)
            line = raw.count("\n", 0, m.start()) + 1
            defs.setdefault(name, []).append(
                {
                    "name": name,
                    "owner": owner,
                    "file": rel,
                    "line": line,
                    "sig": " ".join(sig.split()),
                    "body": body,
                    "src": clean,
                    "key": f"{owner}::{name}" if owner else name,
                    "test": (
                        "/tests/" in rel
                        or rel.endswith("_test.rs")
                        or any(o < m.start() < c for o, c in test_blocks)
                    ),
                }
            )
    return defs


CALL_RE = re.compile(
    r"(?:(?P<dot>\.)|(?P<path>\b(?P<ty>[A-Za-z_][A-Za-z0-9_]*)\s*::\s*))?"
    r"\b(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\("
)


def callees(caller: dict, by_name: dict) -> set:
    """Resolve one body's calls to definition keys, by call kind."""
    hits = set()
    src = caller["src"]
    for m in CALL_RE.finditer(caller["body"]):
        name = m.group("name")
        cands = by_name.get(name)
        if not cands:
            continue
        if m.group("path"):
            ty = m.group("ty")
            for d in cands:
                if d["owner"] == ty:
                    hits.add(d["key"])
        elif m.group("dot"):
            # Receiver type is not recoverable without inference; accept a
            # method definition only when its owner type is named in this file.
            for d in cands:
                if d["owner"] and re.search(rf"\b{re.escape(d['owner'])}\b", src):
                    hits.add(d["key"])
        else:
            for d in cands:
                if d["owner"] is None:
                    hits.add(d["key"])
    return hits


def signature_shape(sig: str, body: str) -> list[str]:
    shape = []
    ret = sig.split("->", 1)[1] if "->" in sig else ""
    ret = " ".join(ret.split())
    if re.search(r"->\s*bool\b", sig):
        shape.append("-> bool")
    if re.search(r"Result\s*<\s*Option", ret):
        shape.append("Result<Option<_>>")
    elif ret.startswith("Option"):
        shape.append("-> Option")
    if re.search(r"Result\s*<\s*Vec", ret):
        shape.append("Result<Vec<_>>")
    if "libc::" in body:
        shape.append("raw libc")
    if "readdir" in body or "read_dir" in body:
        shape.append("directory iterator")
    if re.search(r"map_err|\.ok\(\)|let Ok\(|if let Ok|unwrap_or", body):
        shape.append("error conversion")
    return shape or ["(other)"]


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--all", action="store_true", help="print every reachable fn, not only leaves")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--include-tests", action="store_true")
    ap.add_argument("--report-dynamic", action="store_true")
    ap.add_argument("--paths", action="store_true", help="show one shortest call path per leaf")
    ap.add_argument("--entry", action="append", default=None)
    args = ap.parse_args()

    defs = index_defs()
    by_name = defs
    by_key = {}
    for cands in defs.values():
        for d in cands:
            by_key.setdefault(d["key"], []).append(d)
    entries = args.entry or ENTRY_POINTS

    missing = [e for e in entries if e not in defs]
    if missing:
        print(f"entry point(s) not found: {missing}", file=sys.stderr)
        return 2

    seen = set(entries)
    queue = list(entries)
    parent = {e: None for e in entries}
    while queue:
        key = queue.pop()
        for d in by_key.get(key, []):
            if d["test"] and not args.include_tests:
                continue
            for c in callees(d, by_name):
                if c not in seen:
                    seen.add(c)
                    parent[c] = key
                    queue.append(c)

    def path_to(key: str) -> list:
        chain, cur, guard = [], key, 0
        while cur is not None and guard < 40:
            chain.append(cur)
            cur = parent.get(cur)
            guard += 1
        return list(reversed(chain))

    rows = []
    for key in sorted(seen):
        for d in by_key.get(key, []):
            if d["test"] and not args.include_tests:
                continue
            touches = sorted({t for t in SYSCALL_TOKENS if t in d["body"]})
            if touches or args.all:
                rows.append(
                    {
                        "fn": key,
                        "at": f"{d['file']}:{d['line']}",
                        "shape": signature_shape(d["sig"], d["body"]),
                        "touches": touches,
                        "path": " -> ".join(path_to(key)),
                    }
                )

    if args.report_dynamic:
        dyn_rows = []
        for name in sorted(seen):
            for d in by_key.get(name, []):
                if re.search(r"\bdyn\b|Box<\s*fn|fn\s*\(", d["body"]):
                    dyn_rows.append(f"{d['file']}:{d['line']} {name}")
        print("# residual: reachable bodies using dyn/function values (edges this pass cannot follow)")
        for row in dyn_rows:
            print(row)
        return 0

    if args.json:
        print(json.dumps(rows, indent=2))
        return 0

    print(f"# entry points: {', '.join(entries)}")
    print(f"# reachable fn names: {len(seen)}   syscall-touching leaves: {len(rows)}")
    print()
    for row in rows:
        print(f"{row['at']:>58}  {row['fn']}")
        print(f"{'':>58}    shape: {', '.join(row['shape'])}")
        if row["touches"]:
            print(f"{'':>58}    touches: {', '.join(row['touches'])}")
        if args.paths:
            print(f"{'':>58}    via: {row['path']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
