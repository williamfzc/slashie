"""Check the Slashie docs tree's metadata, index entries, and local links."""

import re
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

DOCS = Path(__file__).resolve().parents[1] / "docs"
LINK = re.compile(r"\[[^\]\n]+\]\(([^)\n]+)\)")
FIELDS = ("type", "title", "description")


def targets(text):
    for match in LINK.finditer(text):
        url = urlsplit(match[1])
        if not url.scheme and not url.netloc and url.path:
            yield unquote(url.path)


def problems(page):
    text = page.read_text(encoding="utf-8")
    lines = text.splitlines()
    if not lines or lines[0] != "---" or "---" not in lines[1:]:
        yield "missing YAML frontmatter"
    else:
        metadata = "\n".join(lines[1 : lines.index("---", 1)])
        for field in FIELDS:
            if not re.search(rf"^{field}:[ \t]+\S", metadata, re.MULTILINE):
                yield f"missing frontmatter field {field}"

    for target in targets(text):
        if not (page.parent / target).exists():
            yield f"broken relative link {target}"

    if page.name != "index.md":
        index = page.with_name("index.md")
        if not index.is_file() or not any(
            (index.parent / target).resolve() == page.resolve()
            for target in targets(index.read_text(encoding="utf-8"))
        ):
            yield f"missing entry in {index.relative_to(DOCS)}"


def main():
    failed = False
    for page in sorted(DOCS.rglob("*.md")):
        for problem in problems(page):
            print(f"docs/{page.relative_to(DOCS)}: {problem}", file=sys.stderr)
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
