"""Move docs/design into one-folder-per-question categories and rewrite every reference.

Run from the repo root: `--moves` does the `git mv`s, no argument rewrites references.
The citation form the maintainer settled on (2026-09-30): a full path from the repo
root plus the section, e.g. `/docs/design/keys/room-device-version.md §4.1`.
"""

import io
import os
import re
import subprocess
import sys

# old basename -> new path relative to the repo root
MOVES = {
    # overview/: what this fork is and how it is laid out
    "fork-overview.md": "docs/design/overview/fork-overview.md",
    "why-not-matrix-and-core-design.md": "docs/design/overview/why-not-matrix-and-core-design.md",
    "repo-structure.md": "docs/design/overview/repo-structure.md",
    "roadmap.md": "docs/design/overview/roadmap.md",
    # build/: how to build and run the tests
    "windows-build.md": "docs/design/build/windows-build.md",
    # wire/: the wbf wire format and the pipeline that carries it
    "wbf-wire-format.md": "docs/design/wire/wire-format.md",
    "wbf-pack-pipeline.md": "docs/design/wire/pack-pipeline.md",
    "wbf-api-bridge.md": "docs/design/wire/api-bridge.md",
    "wbf-vectors.json": "docs/design/wire/wbf-vectors.json",
    # media/: uploads, and who holds a file
    "chunked-upload.md": "docs/design/media/chunked-upload.md",
    "chunked-upload-spec.md": "docs/design/media/chunked-upload-spec.md",
    "media-refcount.md": "docs/design/media/media-refcount.md",
    "media-gc.md": "docs/design/media/media-gc.md",
    "media-holders.md": "docs/design/media/media-holders.md",
    "media-attachments.md": "docs/design/media/media-attachments.md",
    # events/: the timeline, its numbering, and pushing it out
    "room-seq-and-recent.md": "docs/design/events/room-seq-and-recent.md",
    "wbf-event-push.md": "docs/design/events/event-push.md",
    "streaming-messages.md": "docs/design/events/streaming-messages.md",
    # keys/: E2EE
    "wbf-e2ee.md": "docs/design/keys/e2ee-over-channel.md",
    "wbf-to-device.md": "docs/design/keys/to-device.md",
    "wbf-room-device-version.md": "docs/design/keys/room-device-version.md",
    "e2ee-send-guard-problem.md": "docs/design/keys/e2ee-send-guard-problem.md",
    # accounts/: accounts the server owns
    "server-user.md": "docs/design/accounts/server-user.md",
    # history/: kept because the reasoning is worth reading, not as current design
    "review-followups-2026-09-06.md": "docs/design/history/review-followups-2026-09-06.md",
}

SKIP_DIRS = {".git", "target", "node_modules", "scratchpad"}
TEXT_EXT = {".md", ".rs", ".toml", ".ps1", ".py", ".json", ".hcl", ".yml", ".yaml", ".sh"}


def list_repo_files():
    for root, dirs, files in os.walk("."):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            if os.path.splitext(name)[1] in TEXT_EXT:
                yield os.path.normpath(os.path.join(root, name))


def do_moves():
    for old_base, new_path in MOVES.items():
        old_path = os.path.join("docs", "design", old_base)
        if not os.path.exists(old_path):
            print("SKIP (already moved): " + old_path)
            continue
        os.makedirs(os.path.dirname(new_path), exist_ok=True)
        subprocess.check_call(["git", "mv", old_path, new_path])
        print("moved " + old_path + " -> " + new_path)


def fix_prose(text):
    """Rewrite every way a document is named in prose, a comment, or a source path."""
    for old_base, new_path in MOVES.items():
        root = "/" + new_path
        tail = new_path
        # 1. a relative path that walks up to the repo root (include_str!, concat!):
        #    keep the prefix, swap only the tail -- it must stay relative.
        text = re.sub(r"(?<=[./])docs/design/" + re.escape(old_base), tail, text)
        # 2. a path from the repo root, with or without the leading slash
        text = re.sub(r"/?\bdocs/design/" + re.escape(old_base), root, text)
        # 3. a bare file name
        text = re.sub(r"(?<![\w/.-])" + re.escape(old_base) + r"(?![\w-])", root, text)
    return text


def rewrite_one(path):
    with io.open(path, "rb") as handle:
        raw = handle.read()
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return 0
    original = text
    file_dir = os.path.dirname(os.path.normpath(path)) or "."
    stash = []

    if path.endswith(".md"):
        def fix_link(match):
            label, href = match.group(1), match.group(2)
            target, _, anchor = href.partition("#")
            base = os.path.basename(target)
            if "://" in href or base not in MOVES:
                return match.group(0)
            new_path = MOVES[base]
            rel = os.path.relpath(new_path, file_dir).replace(os.sep, "/")
            new_href = rel + ("#" + anchor if anchor else "")
            stash.append("[" + fix_prose(label) + "](" + new_href + ")")
            return "\x00" + str(len(stash) - 1) + "\x00"

        text = re.sub(r"\[([^\]]*)\]\(([^)\s]+)\)", fix_link, text)

    text = fix_prose(text)

    for index, value in enumerate(stash):
        text = text.replace("\x00" + str(index) + "\x00", value)

    if text == original:
        return 0
    with io.open(path, "wb") as handle:
        handle.write(text.encode("utf-8"))
    return 1


def main():
    if "--moves" in sys.argv:
        do_moves()
        return
    changed = 0
    for path in list_repo_files():
        changed += rewrite_one(path)
    print("rewrote " + str(changed) + " files")


if __name__ == "__main__":
    main()
