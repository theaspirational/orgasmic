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

    SELF-CHECKS (orgasmic:TASK-2QK4P.1.1.1.1.1 P2b)
    -----------------------------------------------
    A derivation whose own parse is unchecked answers "nothing else is on the
    path" when it means "my lexer stopped early" — the same collapse the script
    exists to close, moved into the tool. Every run therefore asserts, and
    `--self-check` prints the counts:

    1. THE LEXER CONSUMED TO EOF. `strip_noise` is length-preserving by
       construction, so a mismatch means it fell out of a literal early.
    2. DECLARATION-VS-BODY OWNERSHIP. A signature that ends in `;` (trait
       methods without defaults, `extern "C"` blocks) HAS NO BODY. The old
       next-`{`-after-`fn` rule handed those an unrelated later body — it gave
       `WorkEvidenceProbe::observe` and `setsid` bodies belonging to other
       items, which both inflates the graph and misreports shapes.
    3. EVERY INDEXED ITEM IS ACCOUNTED FOR. Each `fn` the scanner sees is
       classified as exactly one of definition / declaration / unbalanced, and
       the three counts must sum to the number seen. `unbalanced` must be zero.
    4. `cfg(test)` IS EXCLUDED AT ANY VISIBILITY. `pub(crate) mod tests` — which
       is how the api.rs test module is spelled — used to slip through a
       `(?:pub\\s+)?` regex and put the entire test suite in the graph.

    RESIDUAL (stated, not hidden)
    -----------------------------
    - Name-keyed edges cannot resolve trait-object / `dyn` dispatch, function
      values stored in structs, or calls constructed inside macros. Those are
      invisible to this pass; `--report-dynamic` now scans SIGNATURES as well as
      bodies, so a `&dyn WorkerDriver` parameter is reported even when the body
      only calls through it, and lists the `.method(` calls made on each such
      binding so a reader can check them by hand.
    - Over-approximation means the reachable set is a superset. That is stated
      rather than trimmed: a boundary that is too big is reviewable, one that is
      too small is what produced rounds three, four and five.
    - `#[cfg(...)]` other than `cfg(test)` is not evaluated, so non-unix bodies
      are included.
    - THIS IS STILL A LEXER, NOT A RUST PARSER. A `syn`-backed pass (or a
      type-aware graph from `rust-analyzer`) would remove the receiver-inference
      residual outright, and it is the better answer. It was NOT taken in this
      round: neither `tree_sitter`/`tree_sitter_rust` nor any Rust-parsing
      Python module is available in this environment, and the alternative — a
      new workspace crate depending on `syn` plus a build step — is a larger
      change than the finding this round is closing. The self-checks above are
      the bound on the lexer's honesty until then.

Usage:
    python3 scripts/derive-observation-surface.py            # the leaf surface
    python3 scripts/derive-observation-surface.py --all      # every reachable fn
    python3 scripts/derive-observation-surface.py --json     # machine-readable
    python3 scripts/derive-observation-surface.py --self-check
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


RAW_STR_RE = re.compile(r"(?:b|c)?r(?P<hashes>#*)\"")


def strip_noise(text: str) -> str:
    """Blank out string/char literals and line comments so token scans do not
    match documentation prose or embedded snippets.

    Length-preserving by construction: every branch emits exactly as many
    characters as it consumed. `index_defs` asserts that, and asserts the result
    has balanced braces — which is how the RAW-STRING hole below was found. A
    `br#"{"version":1,"tombstoned":{}}"#` fixture has unbalanced braces and
    backslash-escaped quotes that mean nothing inside a raw string, so treating
    it as an ordinary literal both loses the closing quote and injects stray
    `{`/`}` into the brace matcher.
    """
    out = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        rm = (
            RAW_STR_RE.match(text, i)
            if c in "brc" and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_"))
            else None
        )
        if rm:
            terminator = '"' + rm.group("hashes")
            j = text.find(terminator, rm.end())
            j = n if j < 0 else j + len(terminator)
            out.append(" " * (j - i))
            i = j
        elif c == "/" and i + 1 < n and text[i + 1] == "/":
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
    """Return (open_brace, close_brace) of the block that follows `start`.

    `(-1, -1)` when there is no block at all; `(open, -1)` when the block never
    closes, which is a LEXER FAILURE and is asserted on rather than absorbed.
    """
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
    return (open_at, -1)


def signature_end(clean: str, start: int) -> tuple[str, int]:
    """Where one `fn` item's signature ends: `("body", index_of_open_brace)` or
    `("decl", index_of_semicolon)`.

    orgasmic:TASK-2QK4P.1.1.1.1.1 P2b self-check 2 — the old rule was "the next
    `{` after `fn`", which cannot tell a definition from a DECLARATION. A trait
    method without a default (`fn observe(&self, ..) -> bool;`) and an
    `extern "C"` prototype (`fn setsid() -> pid_t;`) both end in `;`, and the
    old rule handed each of them the body of whatever item happened to come
    next. Scanning at bracket depth zero for whichever of `{` / `;` comes first
    is the whole fix.
    """
    depth = 0
    i = start
    n = len(clean)
    while i < n:
        c = clean[i]
        if c in "([":
            depth += 1
        elif c in ")]":
            depth -= 1
        elif depth <= 0:
            if c == "{":
                return ("body", i)
            if c == ";":
                return ("decl", i)
        i += 1
    return ("none", -1)


MOD_RE = re.compile(r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?mod\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\b", re.M)


def index_defs(audit: dict | None = None) -> dict:
    defs = {}
    for path in rust_files():
        raw = open(path, encoding="utf-8", errors="replace").read()
        clean = strip_noise(raw)
        # SELF-CHECK 1: `strip_noise` blanks in place and must be
        # length-preserving. A shorter result means it fell out of a literal
        # early, which silently truncates every offset computed below.
        assert len(clean) == len(raw), (
            f"lexer did not consume {path} to EOF: {len(clean)} != {len(raw)}"
        )
        # And the blanking really did remove every literal: a phantom string
        # that swallowed code would leave the file's braces unbalanced, which is
        # the exact failure mode `strip_noise`'s char-literal branch documents.
        assert clean.count("{") == clean.count("}"), (
            f"unbalanced braces after lexing {path}: the scan lost a literal"
        )
        # Brace-matched impl block ranges, so a free function that merely
        # follows an impl block is not attributed to its type.
        #
        # SELF-CHECK 4: a test module is `mod test`/`mod tests` at ANY
        # visibility, or any module carrying `#[cfg(test)]`. The old
        # `(?:pub\s+)?` missed `pub(crate) mod tests`, which is how api.rs
        # spells its (very large) test module.
        test_blocks = []
        for tm in MOD_RE.finditer(clean):
            head = clean[max(0, tm.start() - 200): tm.start()]
            cfg_test = bool(re.search(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*$", head))
            if tm.group("name") not in ("test", "tests") and not cfg_test:
                continue
            o, c = body_of(clean, tm.end())
            if o >= 0 and c >= 0:
                test_blocks.append((o, c))
        impls = []
        for im in IMPL_RE.finditer(clean):
            o, c = body_of(clean, im.end())
            if o >= 0 and c >= 0:
                impls.append((o, c, im.group("ty")))
        for m in FN_RE.finditer(clean):
            name = m.group("name")
            if audit is not None:
                audit["seen"] += 1
            kind, at = signature_end(clean, m.end())
            # SELF-CHECK 2: a semicolon-only declaration owns no body. Counted,
            # not silently attributed to the next block in the file.
            if kind != "body":
                if audit is not None:
                    audit["declarations" if kind == "decl" else "bodyless"] += 1
                continue
            open_at, close_at = body_of(clean, at)
            assert open_at == at, f"{path}:{name}: signature/body disagree"
            # SELF-CHECK 3 (part): an unbalanced body is a lexer failure, not a
            # function that happens to run to end-of-file.
            assert close_at >= 0, f"unbalanced body for {name} in {path}"
            if audit is not None:
                audit["definitions"] += 1
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
    ap.add_argument(
        "--self-check",
        action="store_true",
        help="print the parse audit (the assertions run on every invocation)",
    )
    ap.add_argument("--paths", action="store_true", help="show one shortest call path per leaf")
    ap.add_argument("--entry", action="append", default=None)
    args = ap.parse_args()

    audit = {"seen": 0, "definitions": 0, "declarations": 0, "bodyless": 0}
    defs = index_defs(audit)
    # SELF-CHECK 3: every `fn` the scanner saw left exactly one accounting
    # entry. A derivation that quietly drops items is an UNDER-approximation,
    # and an under-approximation is the failure this whole task is about.
    accounted = audit["definitions"] + audit["declarations"] + audit["bodyless"]
    assert accounted == audit["seen"], (
        f"unaccounted fn items: saw {audit['seen']}, classified {accounted}"
    )
    assert audit["bodyless"] == 0, (
        f"{audit['bodyless']} fn items ended in neither `{{` nor `;`; the lexer is wrong"
    )
    indexed = sum(len(v) for v in defs.values())
    assert indexed == audit["definitions"], (
        f"index holds {indexed} definitions but {audit['definitions']} were parsed"
    )
    if args.self_check:
        print("# parse self-check")
        for key in ("seen", "definitions", "declarations", "bodyless"):
            print(f"{key:>14}: {audit[key]}")
        print(f"{'indexed names':>14}: {len(defs)}")
        return 0
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
    parent: dict[str, str | None] = {e: None for e in entries}
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
        # orgasmic:TASK-2QK4P.1.1.1.1.1 P2b — SIGNATURES, not only bodies. A
        # function that takes `&dyn WorkerDriver` and calls `driver.acquire(..)`
        # has no `dyn` token in its body at all, so the body-only scan emitted
        # one row and omitted exactly the dispatch a reader has to check by
        # hand. Each row now says WHERE the dynamic value entered (signature or
        # body) and, for a `dyn` parameter, which methods are called on it.
        dyn_rows = []
        param_re = re.compile(
            r"(?P<bind>[A-Za-z_][A-Za-z0-9_]*)\s*:\s*[^,()]*?\bdyn\s+(?P<trait>[A-Za-z_][A-Za-z0-9_]*)"
        )
        for name in sorted(seen):
            for d in by_key.get(name, []):
                if d["test"] and not args.include_tests:
                    continue
                where = []
                if re.search(r"\bdyn\b|Box<\s*fn|fn\s*\(", d["sig"]):
                    where.append("signature")
                if re.search(r"\bdyn\b|Box<\s*fn|fn\s*\(", d["body"]):
                    where.append("body")
                if not where:
                    continue
                calls = set()
                for pm in param_re.finditer(d["sig"]):
                    bind = pm.group("bind")
                    for cm in re.finditer(
                        rf"\b{re.escape(bind)}\b(?:\.as_ref\(\))?\s*\.\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(",
                        d["body"],
                    ):
                        calls.add(f"{pm.group('trait')}::{cm.group(1)}")
                row = f"{d['file']}:{d['line']} {name} [{'+'.join(where)}]"
                if calls:
                    row += "  dynamic calls: " + ", ".join(sorted(calls))
                dyn_rows.append(row)
        print("# residual: reachable fns using dyn/function values (edges this pass cannot follow)")
        print(f"# rows: {len(dyn_rows)}   (signatures and bodies both scanned)")
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
