"""Verify that every e2e script ported to Python checks the same things as its .ps1 (issue #114).

🚨 **A failure here means the two have drifted**, not that this script is wrong: while both exist they
share a name, an output format and an output directory, so a `.py` that silently lost a scenario would
still print `fail=0`. That already happened once -- #113 added scenario 6 to e2e11.ps1 while the port
was in review, and nothing said so.

Compared: the set of check names (`[1.4d]`, ...), both ways. The check text and the condition are not
compared; the names are the contract the README promises ("same check names").

Run from the repo root: `uv run tests/e2e/verify-ports.py`. Exit code 1 lists every difference.
When the last .ps1 is deleted this has nothing left to compare and can go with it.
"""

import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
PS_CHECK = re.compile(r"""^\s*Check\s+['"](\[[^\]'"]+\])""", re.M)
PY_CHECK = re.compile(r"""\bt\.check\(\s*['"](\[[^\]'"]+\])""")


def names(path: pathlib.Path, pattern: re.Pattern) -> list:
    return pattern.findall(path.read_text(encoding="utf-8"))


def main() -> int:
    problems, ported = [], 0
    for py in sorted(HERE.glob("e2e*.py")):
        ps = py.with_suffix(".ps1")
        if not ps.exists():
            continue  # the .ps1 is gone: the port is the only one left
        ported += 1
        a, b = names(ps, PS_CHECK), names(py, PY_CHECK)
        for side, found in ((ps.name, a), (py.name, b)):
            dup = sorted({n for n in found if found.count(n) > 1})
            if dup:
                problems.append(f"{side}: the same check name twice: {', '.join(dup)}")
        only_ps, only_py = sorted(set(a) - set(b)), sorted(set(b) - set(a))
        if only_ps:
            problems.append(f"{py.name}: missing from the port ({len(only_ps)}): {', '.join(only_ps)}")
        if only_py:
            problems.append(f"{py.name}: not in {ps.name} ({len(only_py)}): {', '.join(only_py)}")
        if not only_ps and not only_py:
            print(f"ok {py.name}: {len(set(b))} checks, same names as {ps.name}")
    for p in problems:
        print(f"FAIL {p}")
    if ported == 0:
        print("nothing to compare: no e2e*.py has a .ps1 next to it")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
