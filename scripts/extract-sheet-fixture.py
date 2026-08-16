#!/usr/bin/env python3
"""Extract the real-documentation-excerpt.css fixture from a real rafters sheet.

veneer authors no CSS. A hand-written fixture that *looks like* rafters output
lets tests pass against a shape rafters does not emit -- the fixture ends up
validating veneer's imagination instead of the artifact. Everything this
writes is copied byte-for-byte from a generated sheet, with the source hash
recorded so the fixture's provenance is checkable.

Usage: extract-sheet-fixture.py <path-to-rafters.documentation.css>
"""
import hashlib
import re
import sys

OUT = "crates/veneer-adapters/tests/fixtures/real-documentation-excerpt.css"

# Rules chosen because the contract depends on them: token carriers, a
# composite (shadow) that needs @property, an escaped variant selector, an
# animation that needs @keyframes, and the dark block.
WANTED = [
    r"\.bg-muted\{[^}]*\}",
    r"\.text-foreground\{[^}]*\}",
    r"\.rounded-full\{[^}]*\}",
    r"\.inline-flex\{[^}]*\}",
    r"\.w-full\{[^}]*\}",
    r"\.shadow-sm\{[^}]*\}",
    r"\.hover\\:bg-muted[^{]*\{[^}]*\}",
    r"\.animate-pulse\{[^}]*\}",
    r"@property --tw-shadow\{[^}]*\}",
    r"@keyframes pulse\{.*?\}\}",
]


def main(path: str) -> int:
    sheet = open(path, encoding="utf8").read()
    sha = hashlib.sha256(sheet.encode()).hexdigest()

    parts = [m.group(0) for m in re.finditer(r":host\{[^}]*\}", sheet)]
    for pattern in WANTED:
        found = re.search(pattern, sheet)
        if found:
            parts.append(found.group(0))
    dark = re.search(r"\.dark\{[^}]{0,400}", sheet)
    if dark:
        parts.append(dark.group(0).rsplit(";", 1)[0] + "}")

    body = "".join(parts)
    header = (
        "/* VERBATIM EXCERPT of real rafters output -- not authored by veneer.\n"
        "   Source: apps/demo/.rafters/output/rafters.documentation.css\n"
        f"   Source sha256: {sha}\n"
        "   Extracted by scripts/extract-sheet-fixture.py -- every byte below is\n"
        "   copied from that artifact. veneer authors no CSS, and a fixture that\n"
        "   veneer hand-wrote would let these tests pass against a sheet shape\n"
        "   rafters does not actually emit. */\n"
    )
    open(OUT, "w", encoding="utf8").write(header + body + "\n")
    print(f"wrote {OUT} from a sheet with sha256 {sha}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
