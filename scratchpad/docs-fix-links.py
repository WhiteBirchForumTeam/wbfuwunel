"""Second pass: the links the move broke that were not themselves moved.

A file that went from docs/design/X.md to docs/design/cat/X.md sits one level deeper,
so every *other* relative link in it (../bridge-specs/..., ../../CHANGELOG-fork.md)
now points one level too high. Also rewrites link labels that were written as a path.
"""

import io
import os
import re

OLD_DIR = os.path.join("docs", "design")


def read(path):
    with io.open(path, "rb") as handle:
        return handle.read().decode("utf-8")


def write(path, text):
    with io.open(path, "wb") as handle:
        handle.write(text.encode("utf-8"))


def moved_docs():
    for root, dirs, files in os.walk(OLD_DIR):
        if os.path.normpath(root) == os.path.normpath(OLD_DIR):
            continue
        for name in files:
            if name.endswith(".md"):
                yield os.path.normpath(os.path.join(root, name))


def to_label(path_like, new_target_root):
    """A label written as a path becomes the root citation, keeping any trailing section."""
    section = ""
    match = re.search(r"(\s*§.*)$", path_like)
    if match:
        section = match.group(1)
    return "/" + new_target_root + section


changed = 0
for path in moved_docs():
    text = read(path)
    original = text
    file_dir = os.path.dirname(path)

    def fix(match):
        label, href = match.group(1), match.group(2)
        if "://" in href or href.startswith("#") or href.startswith("mailto:"):
            return match.group(0)
        target, _, anchor = href.partition("#")
        if not target:
            return match.group(0)
        resolved = os.path.normpath(os.path.join(file_dir, target))
        if not os.path.exists(resolved):
            # Try it from where the file used to live; if that is where it pointed,
            # the only thing wrong is the depth.
            from_old = os.path.normpath(os.path.join(OLD_DIR, target))
            if os.path.exists(from_old):
                target = os.path.relpath(from_old, file_dir).replace(os.sep, "/")
                resolved = from_old
            else:
                return match.group(0)
        # A label that is itself a path must name the file from the repo root.
        if "/" in label or label.endswith(".md") or label.endswith(".json"):
            label = to_label(label, os.path.normpath(resolved).replace(os.sep, "/"))
        return "[" + label + "](" + target + ("#" + anchor if anchor else "") + ")"

    text = re.sub(r"\[([^\]]*)\]\(([^)\s]+)\)", fix, text)
    if text != original:
        write(path, text)
        changed += 1

print("fixed links in " + str(changed) + " files")
