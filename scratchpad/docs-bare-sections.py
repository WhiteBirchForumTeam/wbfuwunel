"""Give every bare `§N` in Rust a file name.

A `.rs` file has no "same document" to fall back on, so a bare `§N` there can only
be resolved by knowing which document the module follows -- and a grep for that
document's path will not find the line. That is the failure the citation rule exists
to prevent, so each one gets the full path.

Skipped: a `§N` whose citation already carries a path on this line or the line above
(a wrapped citation is complete, just broken across lines), and RFC/MSC sections.
"""

import io
import re

# file -> the document its bare sections belong to
OWNER = {
    "src/api/client/membership/members.rs": "/docs/design/keys/room-device-version.md",
    "src/api/client/send.rs": "/docs/design/keys/room-device-version.md",
    "src/api/client/wbf/device.rs": "/docs/design/keys/to-device.md",
    "src/api/client/wbf/mod.rs": "/docs/design/wire/wire-format.md",
    "src/api/client/wbf/send.rs": "/docs/design/keys/room-device-version.md",
    "src/api/client/wbf/session.rs": "/docs/design/wire/wire-format.md",
    "src/api/client/wbf/ws.rs": "/docs/design/wire/wire-format.md",
    "src/api/client/wbf/recent.rs": "/docs/design/wire/pack-pipeline.md",
    "src/core/wbf/error_code.rs": "/docs/design/wire/wire-format.md",
    "src/core/wbf/id_type.rs": "/docs/design/wire/wire-format.md",
    "src/core/wbf/vectors.rs": "/docs/design/wire/wire-format.md",
    "src/service/connections.rs": "/docs/design/wire/pack-pipeline.md",
    "src/service/device_versions/mod.rs": "/docs/design/keys/room-device-version.md",
    "src/service/device_versions/keys_hash.rs": "/docs/design/keys/room-device-version.md",
    "src/service/streams/rooms.rs": "/docs/design/keys/room-device-version.md",
}

# Lines where the section belongs to a different document than the file's owner.
OVERRIDE = {
    ("src/api/client/wbf/draft.rs", "§7"): "/docs/design/events/streaming-messages.md",
    ("src/api/client/wbf/draft.rs", "§3.4"): "/docs/design/wire/wire-format.md",
}

PATH_MARK = "/docs/design/"
SECTION = re.compile(r"(?<![\w./])§([0-9]+(?:\.[0-9]+)?)")

total = 0
for path in sorted(set(list(OWNER) + [key[0] for key in OVERRIDE])):
    with io.open(path, "rb") as handle:
        lines = handle.read().decode("utf-8").split("\r\n")
    hits = 0
    for index, line in enumerate(lines):
        if "§" not in line or "RFC" in line or "MSC" in line:
            continue
        previous = lines[index - 1] if index else ""
        if PATH_MARK in line or PATH_MARK in previous:
            continue

        def name(match):
            global hits
            key = (path, "§" + match.group(1))
            target = OVERRIDE.get(key, OWNER.get(path))
            if target is None:
                return match.group(0)
            hits += 1
            return target + " §" + match.group(1)

        lines[index] = SECTION.sub(name, line)
    if hits:
        with io.open(path, "wb") as handle:
            handle.write("\r\n".join(lines).encode("utf-8"))
        print(path + ": " + str(hits))
        total += hits

print("named " + str(total) + " bare sections")
