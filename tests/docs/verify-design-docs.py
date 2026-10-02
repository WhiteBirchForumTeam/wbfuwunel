"""Verify the design docs' paths and citations.

🚨 **A failure here means a reference is lying**, not that this script is wrong:
  - DEAD LINK    a markdown link points at a file that is not there.
  - STALE NAME   a document's pre-2026-09-30 file name is still written somewhere.
  - STALE PATH   a path from before the reorganisation (no category folder).
  - NO SUCH §   a citation names a section the target document does not have.
                 🚨 This is the one that catches content loss: if a document is
                 truncated, the sections other docs cite stop existing.
  - NOT IN INDEX a design doc no one can find from /docs/design/index.md.
  - NO SUCH DOC  prose names a /docs/... path that is not in this repo. A path
                 in another repo must say so and name the repo, so that a
                 reader is not sent looking for a local file.

Run from the repo root: `python tests/docs/verify-design-docs.py`. Exit code 1 lists
every problem. CLAUDE.md D asks for a script that walks the whole repo after a rename,
because a stale name in a link or a comment never fails the build -- this is it.
"""

import io
import os
import re
import subprocess
import sys

SKIP_DIRS = {".git", "target", "node_modules", "scratchpad"}
SELF = os.path.normpath(__file__)
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


def list_indexed_symlinks():
    """
    Return:
        set[str]  normalised paths git records as symlinks (mode 120000);
        empty when git cannot be asked, and the caller falls back to guessing.

    ⭐ The index is the authoritative answer and it is the same on every
    platform, which is the whole point: the shape on disk is not (see
    `is_symlink_stub`). Asking it is also the lesson of how this check came to
    be wrong in the first place — the author measured the symlink's own blob
    (18 bytes of `../CONTRIBUTING.md`) and read it as the file's content
    (PR #103).
    """
    try:
        listing = subprocess.check_output(["git", "ls-files", "-s", "-z"], stderr=subprocess.PIPE)
    except (OSError, subprocess.CalledProcessError):
        return set()
    found = set()
    for entry in listing.decode("utf-8", "replace").split("\0"):
        if not entry.startswith("120000 "):
            continue
        # `<mode> <object> <stage>\t<path>`
        parts = entry.split("\t", 1)
        if len(parts) == 2:
            found.add(os.path.normpath(parts[1]))
    return found


INDEXED_SYMLINKS = list_indexed_symlinks()


def is_symlink_stub(path):
    """
    Args:
        path: a file this walk found, example: "docs/contributing.md"
    Return:
        bool  True when the file is a symbolic link to another file in this
        repo, under either checkout shape; otherwise False.

    🚨 Two files here are symlinks the fork inherited (`development.md` ->
    `docs/development.md`, `docs/contributing.md` -> `../CONTRIBUTING.md`, both
    mode 120000). Their links belong to the *target*'s directory, but a walk
    resolves them against the *link*'s directory, which invents dead links that
    no reader can see -- and ⭐ it did so on Linux only, because a Windows
    checkout with `core.symlinks=false` writes the target path into a plain
    file instead, where there is nothing to resolve. A check that answers
    differently per platform is a lying observable: it reported ALL CLEAR to
    the author and two dead links to review (cirno, PR #103).
    """
    if path in INDEXED_SYMLINKS:
        return True
    if os.path.islink(path):
        return True
    # Last resort, when there is no git to ask: the `core.symlinks=false` shape,
    # where the whole file is one relative path. ⚠️ A real document whose entire
    # content is a path would be skipped too (review, rumia) — it cannot hold a
    # markdown link, so the link check loses nothing, but a stale name in it
    # would go unseen. That is why the index is asked first.
    if INDEXED_SYMLINKS:
        return False
    try:
        with io.open(path, "r", encoding="utf-8") as handle:
            body = handle.read(512)
    except (IOError, OSError, UnicodeDecodeError):
        return False
    if "\n" in body.strip() or not body.strip():
        return False
    target = os.path.join(os.path.dirname(path) or ".", body.strip())
    return os.path.isfile(target)


def walk_files(exts):
    for root, dirs, files in os.walk("."):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            path = os.path.normpath(os.path.join(root, name))
            if os.path.splitext(name)[1] not in exts:
                continue
            if os.path.abspath(path) == os.path.abspath(SELF):
                continue
            if is_symlink_stub(path):
                continue
            yield path


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

# 3. a /docs/... path written in prose must exist here, or say which repo it is in
OTHER_REPO_MARKS = ("amaid/wbf-matrix-client", "另一個 repo", "client repo")
URL = re.compile(r"https?://\S+")
for path in walk_files({".md", ".rs", ".toml", ".ps1"}):
    lines = read(path).splitlines()
    for line_no, line in enumerate(lines, 1):
        # A path inside a URL belongs to some other project's site, not to this repo.
        bare = URL.sub(" ", line)
        if not bare.count("/docs/"):
            continue
        # The marker may sit a line or two above: a citation wraps.
        nearby = " ".join(lines[max(0, line_no - 3):line_no + 1])
        if any(mark in nearby for mark in OTHER_REPO_MARKS):
            continue
        for match in re.finditer(r"/docs/[A-Za-z0-9._/-]+\.(?:md|json)", bare):
            target = match.group(0).lstrip("/")
            if not os.path.exists(target):
                problems.append(
                    "NO SUCH DOC " + path + ":" + str(line_no) + "  " + match.group(0)
                )

# 4. a cited section must exist in the document being cited
HEADING = re.compile(r"^#{1,6}\s+([0-9]+(?:\.[0-9]+)*)\.?\s")
sections_of = {}
for path in walk_files({".md"}):
    found = set()
    for line in read(path).splitlines():
        match = HEADING.match(line)
        if match:
            number = match.group(1)
            found.add(number)
            found.add(number.split(".")[0])
    sections_of[os.path.normpath(path).replace(os.sep, "/")] = found

CITATION = re.compile(r"(/docs/[A-Za-z0-9._/-]+\.md)`?\s*§([0-9]+(?:\.[0-9]+)*)")
for path in walk_files({".md", ".rs", ".ps1", ".toml"}):
    for line_no, line in enumerate(read(path).splitlines(), 1):
        for target, section in CITATION.findall(line):
            local = target.lstrip("/")
            known = sections_of.get(local)
            if known is None or not known:
                continue
            if section not in known:
                problems.append(
                    "NO SUCH §  " + path + ":" + str(line_no)
                    + "  " + target + " §" + section
                )

# 5. every design doc must be reachable from the index
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
