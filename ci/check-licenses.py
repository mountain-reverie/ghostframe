#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Licensing guard. The policy is in AGENTS.md ("Licensing"); this checks it.

1. Every workspace crate declares `license` in its Cargo.toml, and it is one
   of the two project licences: AGPL-3.0-or-later (the server side) or
   MIT OR Apache-2.0 (everything else).
2. Copyleft only flows one way: a crate with a normal or build dependency on
   an AGPL crate must itself be AGPL. A client crate that started depending on
   ghostframe-lib would quietly make the client AGPL; this fails instead.
3. Every third-party crate in the normal dependency closure of a workspace
   crate (what is linked into a binary; dev and build dependencies are not)
   has a licence expression satisfiable from PERMISSIVE, or, when only AGPL
   crates link it, from PERMISSIVE plus AGPL_ONLY_EXTRA. A new
   copyleft or unknown licence is a decision, not a dependency bump: add it
   here deliberately, with a comment.
4. Every tracked source file carries `SPDX-License-Identifier: <licence>` in
   its first lines, and that licence is the one of the crate that owns the
   file (or MIT OR Apache-2.0 outside any crate), unless the file is listed
   in FILE_EXCEPTIONS (third-party code, see NOTICE).

Runs `cargo metadata` (no build) and `git ls-files`, so it is cheap; `just
ci-local`, `just ci-client` and CI's clippy job all run it.
"""

import json
import re
import subprocess
import sys
from pathlib import Path

AGPL = "AGPL-3.0-or-later"
PERMISSIVE_PROJECT = "MIT OR Apache-2.0"
PROJECT_LICENSES = {AGPL, PERMISSIVE_PROJECT}

# Licences a shipped third-party crate may be under (any one alternative of an
# OR is enough; every operand of an AND must be here). All are compatible with
# both MIT OR Apache-2.0 and AGPL-3.0-or-later.
PERMISSIVE = {
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "0BSD",
    "Unlicense",
    "BSL-1.0",
    "CC0-1.0",
    "Unicode-3.0",
    "NCSA",
    "CDLA-Permissive-2.0",
    # ffmpeg-next / ffmpeg-sys-next: the Rust bindings only. libav* itself is
    # linked dynamically from the system (LGPL-2.1+, or GPL for a distribution
    # built with --enable-gpl); that is the end user's build, not ours.
    "WTFPL",
}

# Licences additionally allowed for a crate linked only into AGPL crates.
# MPL-2.0 section 3.3 lets MPL code be combined into a (A)GPL-3.0 work
# (webdriver, via fantoccini, in the ghostframe-e2e harness). Not allowed
# into the permissive side, where it would add file-level copyleft
# obligations for whoever embeds the client.
AGPL_ONLY_EXTRA = {"MPL-2.0"}

# Crates that ship a licence file instead of an SPDX expression.
LICENSE_FILE_CRATES = {
    # Bluebugs/goog_cc, a port of WebRTC's goog_cc: WebRTC's BSD-3-Clause
    # LICENSE (and its PATENTS grant) at the pinned rev.
    "goog_cc": "BSD-3-Clause",
}

# Files whose licence is not their crate's: third-party code, listed in NOTICE.
FILE_EXCEPTIONS = {
    "ghostframe-lib/src/transport/delivery_rate.rs": "Apache-2.0",
}

SOURCE_SUFFIXES = {
    ".rs", ".go", ".ts", ".mts", ".mjs", ".js",
    ".wgsl", ".comp", ".c", ".h", ".py", ".sh",
}
SPDX_RE = re.compile(r"SPDX-License-Identifier:\s*(.+?)\s*(?:\*/)?\s*$")
HEADER_LINES = 5

ROOT = Path(__file__).resolve().parent.parent


def cargo_metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


def tokenize(expr):
    # Legacy "MIT/Apache-2.0" means OR.
    expr = expr.replace("/", " OR ")
    return re.findall(r"\(|\)|[^\s()]+", expr)


def satisfiable(expr, allowed=PERMISSIVE):
    """True if the SPDX expression can be met using `allowed` licences only."""
    tokens = tokenize(expr)
    pos = 0

    def atom():
        nonlocal pos
        if tokens[pos] == "(":
            pos += 1
            value = or_expr()
            pos += 1  # ")"
            return value
        name = tokens[pos]
        pos += 1
        if pos < len(tokens) and tokens[pos] == "WITH":
            name = f"{name} WITH {tokens[pos + 1]}"
            pos += 2
            # An exception only adds permissions to a licence.
            return name in allowed or name.split(" WITH ")[0] in allowed
        return name in allowed

    def and_expr():
        nonlocal pos
        value = atom()
        while pos < len(tokens) and tokens[pos] == "AND":
            pos += 1
            value = atom() and value
        return value

    def or_expr():
        nonlocal pos
        value = and_expr()
        while pos < len(tokens) and tokens[pos] == "OR":
            pos += 1
            value = and_expr() or value
        return value

    return or_expr()


def main():
    errors = []
    meta = cargo_metadata()
    packages = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}

    # 1. Every workspace crate declares a project licence.
    crate_dirs = {}
    for pid in members:
        pkg = packages[pid]
        lic = pkg.get("license")
        if lic not in PROJECT_LICENSES:
            errors.append(
                f"{pkg['name']}: license = {lic!r}; a workspace crate must declare "
                f"{AGPL!r} or {PERMISSIVE_PROJECT!r} (see AGENTS.md, Licensing)"
            )
        crate_dirs[str(Path(pkg["manifest_path"]).parent.relative_to(ROOT))] = lic

    def deps(pid, kinds):
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] in kinds for k in dep["dep_kinds"]):
                yield dep["pkg"]

    # 2. Nothing permissive depends on anything AGPL.
    for pid in members:
        pkg = packages[pid]
        if pkg.get("license") == AGPL:
            continue
        for dep in deps(pid, {None, "build"}):
            if dep in members and packages[dep].get("license") == AGPL:
                errors.append(
                    f"{pkg['name']} ({pkg.get('license')}) depends on "
                    f"{packages[dep]['name']} ({AGPL}); a crate that links AGPL "
                    "code must itself be AGPL, or the dependency must go"
                )

    # 3. Third-party crates linked into anything we build are permissive
    #    (or, linked only into the AGPL side, AGPL-compatible).
    def closure(roots):
        seen, stack = set(), list(roots)
        while stack:
            pid = stack.pop()
            for dep in deps(pid, {None}):
                if dep not in seen:
                    seen.add(dep)
                    stack.append(dep)
        return seen - members

    permissive_side = closure(p for p in members if packages[p].get("license") != AGPL)
    agpl_side = closure(p for p in members if packages[p].get("license") == AGPL)
    third_party = permissive_side | agpl_side
    for pid in sorted(third_party, key=lambda p: packages[p]["name"]):
        pkg = packages[pid]
        allowed = PERMISSIVE if pid in permissive_side else PERMISSIVE | AGPL_ONLY_EXTRA
        side = "client/shared" if pid in permissive_side else "AGPL-only"
        lic = pkg.get("license") or LICENSE_FILE_CRATES.get(pkg["name"])
        if lic is None:
            errors.append(
                f"third-party {pkg['name']} {pkg['version']} has no SPDX licence; "
                "read its licence file and add it to LICENSE_FILE_CRATES"
            )
        elif not satisfiable(lic, allowed):
            errors.append(
                f"third-party {pkg['name']} {pkg['version']} is {lic!r}, linked into the "
                f"{side} side, which does not allow it (allowlists in ci/check-licenses.py)"
            )

    # 4. SPDX headers match the owning crate.
    files = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.split("\0")
    by_depth = sorted(crate_dirs, key=lambda d: -len(d))
    missing = []
    for rel in files:
        if not rel or Path(rel).suffix not in SOURCE_SUFFIXES:
            continue
        want = FILE_EXCEPTIONS.get(rel)
        if want is None:
            owner = next((d for d in by_depth if rel.startswith(d + "/")), None)
            want = crate_dirs[owner] if owner else PERMISSIVE_PROJECT
        try:
            with open(ROOT / rel, encoding="utf-8", errors="replace") as f:
                head = [next(f, "") for _ in range(HEADER_LINES)]
        except FileNotFoundError:
            continue  # deleted in the work tree, not yet staged
        found = None
        for line in head:
            m = SPDX_RE.search(line)
            if m:
                found = m.group(1)
                break
        if found is None:
            missing.append(rel)
        elif found != want:
            errors.append(f"{rel}: SPDX-License-Identifier is {found!r}, expected {want!r}")
    if missing:
        errors.append(
            f"{len(missing)} source file(s) without an SPDX-License-Identifier line in "
            f"their first {HEADER_LINES} lines:\n    " + "\n    ".join(missing)
        )

    for e in errors:
        print(f"::error::{e}")
    if errors:
        print("licence check failed (see ci/check-licenses.py and AGENTS.md, Licensing)")
        return 1
    print(f"licences OK: {len(members)} crates, {len(third_party)} third-party crates")
    return 0


if __name__ == "__main__":
    sys.exit(main())
