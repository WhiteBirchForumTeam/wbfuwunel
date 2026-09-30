"""Verify the docs reorganisation: every markdown link resolves, no stale names remain.

Run from the repo root. Exit code 1 and a list of problems, or "ALL CLEAR".
CLAUDE.md D says a rename is only done when a script has confirmed every reference
still resolves -- this is that script.
"""

import io
import os
import re
import sys

SKIP_DIRS = {".git", "target", "node_modules", "scratchpad"}
OLD_NAMES = [
    "wbf-wire-format.md", "wbf-pack-pipeline.md", "wbf-api-bridge.md", "wbf-e2ee.md",
    "wbf-to-device.md", "wbf-room-device-version.md", "wbf-event-push.md",
]
# These keep their basename, so only a path without the category folder is stale.
STALE_PATHS = [
    "docs/design/fork-overview.md", "docs/design/why-not-matrix-and-core-design.md",
    "docs/design/repo-structure.md", "docs/design/roadmap.md",
    "docs/design/windows-build.md", "docs/design/chunked-upload.md",
    "docs/design/chunked-upload-spec.md", "docs/design/media-refcount.md",
    "docs/design/media-gc.md", "docs/design/media-holders.md",
    "docs/design/media-attachments.md", "docs/design/room-seq-and-recent.md",
    "docs/design/streaming-messages.md", "docs/design/e2ee-send-guard-problem.md",
    "docs/design/server-user.md", "docs/design/review-followups-2026-09-06.md",
    "docs/design/wbf-vectors.json",
]

problems = []


def walk_files(exts):
    for root, dirs, files in os.walk("."):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            if os.path.splitext(name)[1] in exts:
                yield os.path.normpath(os.path.join(root, name))


def read(path):
    with io.open(path, "rb") as handle:
        try:
            return handle.read().decode("utf-8")
        except UnicodeDecodeError:
            return ""


# 1. every relative markdown link into the repo must resolve to a real file
for path in walk_files({".md"}):
    text = read(path)
    file_dir = os.path.dirname(path) or "."
    for match in re.finditer(r"\[[^\]]*\]\(([^)\s]+)\)", text):
        href = match.group(1)
        if "://" in href or href.startswith("#") or href.startswith("mailto:"):
            continue
        target = href.split("#")[0]
        if not target:
            continue
        resolved = os.path.normpath(os.path.join(file_dir, target))
        if not os.path.exists(resolved):
            problems.append("DEAD LINK  " + path + "  ->  " + href)

# 2. no old basename may survive anywhere
for path in walk_files({".md", ".rs", ".toml", ".ps1", ".py", ".json", ".hcl", ".yml"}):
    text = read(path)
    for line_no, line in enumerate(text.splitlines(), 1):
        for old in OLD_NAMES:
            if old in line:
                problems.append("STALE NAME " + path + ":" + str(line_no) + "  " + old)
        for stale in STALE_PATHS:
            if stale in line:
                problems.append("STALE PATH " + path + ":" + str(line_no) + "  " + stale)

# 3. every design doc must be reachable from the index
index_path = os.path.join("docs", "design", "index.md")
if not os.path.exists(index_path):
    problems.append("MISSING    docs/design/index.md")
else:
    index = read(index_path)
    for path in walk_files({".md", ".json"}):
        parts = os.path.normpath(path).split(os.sep)
        if len(parts) < 5 or parts[1:3] != ["docs", "design"]:
            continue
        if parts[-1] == "index.md":
            continue
        root_path = "/" + "/".join(parts[1:])
        if root_path not in index:
            problems.append("NOT IN INDEX  " + root_path)

if problems:
    for item in sorted(set(problems)):
        print(item)
    print("")
    print(str(len(set(problems))) + " problem(s)")
    sys.exit(1)
print("ALL CLEAR")
