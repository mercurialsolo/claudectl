"""Check that every relative link and anchor in the Markdown resolves.

The docs workflow only runs `mkdocs gh-deploy` after merge, so nothing caught a
broken relative link before it shipped. Three were already broken when this was
written, including one from `CLAUDE.md` to a section that had never existed.

Anchors are checked too, since a renamed heading keeps no anchor — that is how
`docs/relay.md` came to link to a section of itself that no longer existed.

Slugs follow GitHub's rule, which matters in two places that bit us: runs of
whitespace are *not* collapsed, so `A — B` yields a doubled hyphen; and an
explicit `<a id="...">` counts as an anchor, which `docs/open-cluster.md` uses
for its Q&A entries.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
FILES = [
    str(p.relative_to(ROOT))
    for p in sorted(ROOT.glob("*.md")) + sorted(ROOT.glob("docs/**/*.md"))
]

LINK = re.compile(r"\[[^\]]*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")


def anchors_of(path: pathlib.Path) -> set[str]:
    out = set()
    for line in path.read_text().splitlines():
        m = re.match(r"#+\s+(.*)", line)
        if not m:
            continue
        slug = m.group(1).strip().lower()
        slug = re.sub(r"[`*_]", "", slug)
        slug = re.sub(r"[^\w\s-]", "", slug)
        slug = slug.replace(" ", "-")
        out.add(slug)
    # Explicit HTML anchors count too — docs/open-cluster.md uses them for its
    # Q&A entries, which have no heading of their own.
    out.update(re.findall(r'<a id="([^"]+)"', path.read_text()))
    return out


bad = []
for rel in FILES:
    src = ROOT / rel
    for target in LINK.findall(src.read_text()):
        if target.startswith(("http://", "https://", "mailto:", "cctl://")):
            continue
        path_part, _, anchor = target.partition("#")
        if not path_part:
            # Same-file anchor.
            if anchor and anchor not in anchors_of(src):
                bad.append(f"{rel}: anchor #{anchor} not found in itself")
            continue
        dest = (src.parent / path_part).resolve()
        if not dest.exists():
            bad.append(f"{rel}: {target} -> missing {dest}")
            continue
        if anchor and dest.suffix == ".md" and anchor not in anchors_of(dest):
            bad.append(f"{rel}: {target} -> {dest.name} has no anchor #{anchor}")

if bad:
    print(f"{len(bad)} broken:")
    for b in bad:
        print("  ", b)
    sys.exit(1)
print(f"all relative links and anchors resolve across {len(FILES)} files")
