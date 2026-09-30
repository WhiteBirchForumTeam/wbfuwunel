"""Rewrite alias citations to the full path form the maintainer settled on (2026-09-30).

An alias names a document without its path -- `wire-format §3.4`, `pipeline §6.2`,
`bridge-specs/index.md` -> `index §1.5`, and the Chinese ones. A grep for the full
path has to find every mention, so every alias becomes a full path.
"""

import io
import os
import re

# Longest first: `chunked-upload-spec` must win over `chunked-upload`.
ALIASES = [
    ("wbf-wire-format", "/docs/design/wire/wire-format.md"),
    ("wire-format", "/docs/design/wire/wire-format.md"),
    ("wbf-pack-pipeline", "/docs/design/wire/pack-pipeline.md"),
    ("pack-pipeline", "/docs/design/wire/pack-pipeline.md"),
    ("pipeline", "/docs/design/wire/pack-pipeline.md"),
    ("wbf-api-bridge", "/docs/design/wire/api-bridge.md"),
    ("api-bridge", "/docs/design/wire/api-bridge.md"),
    ("wbf-room-device-version", "/docs/design/keys/room-device-version.md"),
    ("room-device-version", "/docs/design/keys/room-device-version.md"),
    ("wbf-to-device", "/docs/design/keys/to-device.md"),
    ("wbf-e2ee", "/docs/design/keys/e2ee-over-channel.md"),
    ("wbf-event-push", "/docs/design/events/event-push.md"),
    ("event-push", "/docs/design/events/event-push.md"),
    ("chunked-upload-spec", "/docs/design/media/chunked-upload-spec.md"),
    ("chunked-upload", "/docs/design/media/chunked-upload.md"),
    ("media-attachments", "/docs/design/media/media-attachments.md"),
    ("media-holders", "/docs/design/media/media-holders.md"),
    ("media-refcount", "/docs/design/media/media-refcount.md"),
    ("media-gc", "/docs/design/media/media-gc.md"),
    ("room-seq-and-recent", "/docs/design/events/room-seq-and-recent.md"),
    ("streaming-messages", "/docs/design/events/streaming-messages.md"),
    ("review-followups-2026-09-06", "/docs/design/history/review-followups-2026-09-06.md"),
    ("review-followups", "/docs/design/history/review-followups-2026-09-06.md"),
    ("why-not-matrix-and-core-design", "/docs/design/overview/why-not-matrix-and-core-design.md"),
    ("repo-structure", "/docs/design/overview/repo-structure.md"),
    ("fork-overview", "/docs/design/overview/fork-overview.md"),
    ("windows-build", "/docs/design/build/windows-build.md"),
    ("server-user", "/docs/design/accounts/server-user.md"),
    # Chinese aliases, each resolved by reading where it is used.
    ("橋的設計", "/docs/design/wire/api-bridge.md"),
    ("問題書", "/docs/design/keys/e2ee-send-guard-problem.md"),
    ("核心設計", "/docs/design/overview/why-not-matrix-and-core-design.md"),
    ("分配表", "/docs/design/wire/wire-format.md"),
]

# `spec §12` and `index §1.5` are too generic to put in the table above: `spec` also
# means the Matrix spec, and `index` also means a database index. Both are rewritten
# by their exact surrounding text instead.
LITERALS = [
    ("（spec §12）", "（/docs/design/media/chunked-upload-spec.md §12）"),
    ("也寫在 spec §12", "也寫在 /docs/design/media/chunked-upload-spec.md §12"),
    ("見 index §1.5", "見 /docs/bridge-specs/index.md §1.5"),
    ("在 index §1.5", "在 /docs/bridge-specs/index.md §1.5"),
    ("`docs/bridge-specs/index.md` §", "`/docs/bridge-specs/index.md` §"),
    ("`docs/design/", "`/docs/design/"),
]

SKIP_DIRS = {".git", "target", "node_modules", "scratchpad"}
SKIP_FILES = {os.path.join("docs", "design", "index.md")}  # it quotes the aliases as examples
TEXT_EXT = {".md", ".rs", ".toml", ".ps1", ".py"}

changed = 0
for root, dirs, files in os.walk("."):
    dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
    for name in files:
        if os.path.splitext(name)[1] not in TEXT_EXT:
            continue
        path = os.path.normpath(os.path.join(root, name))
        if path.replace("./", "") in SKIP_FILES or os.path.normpath(path) in {os.path.normpath(os.path.join(".", s)) for s in SKIP_FILES}:
            continue
        with io.open(path, "rb") as handle:
            try:
                text = handle.read().decode("utf-8")
            except UnicodeDecodeError:
                continue
        original = text
        for alias, target in ALIASES:
            # `<alias> §N`, where the alias is not already part of a path or a longer word.
            text = re.sub(
                r"(?<![\w/.`-])" + re.escape(alias) + r"(?= §[0-9])",
                target,
                text,
            )
            # The same alias inside backticks: `wire-format` §3.4
            text = re.sub(
                r"`" + re.escape(alias) + r"`(?= §[0-9])",
                "`" + target + "`",
                text,
            )
        for old, new in LITERALS:
            text = text.replace(old, new)
        if text != original:
            with io.open(path, "wb") as handle:
                handle.write(text.encode("utf-8"))
            changed += 1

print("rewrote aliases in " + str(changed) + " files")
