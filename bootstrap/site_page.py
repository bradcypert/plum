#!/usr/bin/env python3
"""Turns one repository Markdown file into one Hugo content page.

The prose on plumlang.org is not written twice. Each page is generated
from the Markdown at the repository root, so `LANGUAGE.md` reads
correctly on GitHub and on the site without anyone keeping two copies in
step.

What has to be adjusted is the LINKS, because the two contexts disagree
about what a relative path means. On GitHub `[Modules](MODULES.md)`
resolves; on the site it has to be `/modules/`. And a link to a source
file has no page at all on the site, so it becomes a link to the file on
GitHub rather than a dead relative path.

    bootstrap/site_page.py LANGUAGE.md language "Language tour" "..." true
"""
import re
import sys

REPO = "https://github.com/bradcypert/plum"

# Root documents that ARE published as site pages.
PAGES = {
    "TUTORIAL.md": "/tutorial/",
    "GRAMMAR.md": "/grammar/",
    "VISION.md": "/vision/",
    "PORTING.md": "/porting/",
    "LANGUAGE.md": "/language/",
    "MODULES.md": "/modules/",
    "TOOLING.md": "/tooling/",
    "RUNNING.md": "/running/",
    "INSTALL.md": "/install/",
}

LINK = re.compile(r"\]\(([^)]+)\)")


def rewrite(target: str) -> str:
    """One link target, as the site should see it."""
    if target.startswith(("http://", "https://", "mailto:", "#", "/")):
        return target

    path, _, anchor = target.partition("#")
    anchor = f"#{anchor}" if anchor else ""

    if path in PAGES:
        return PAGES[path] + anchor

    # The generated API reference. `docs/stdlib/` is the Markdown copy
    # in the repository; the site serves the HTML `plum doc` produced.
    if path == "docs/stdlib/" or path == "docs/stdlib":
        return "/api/index.html" + anchor
    if path.startswith("docs/stdlib/") and path.endswith(".md"):
        return "/api/" + path[len("docs/stdlib/"):-len(".md")] + ".html" + anchor

    # Everything else is a file in the repository -- an example, a shim,
    # an editor config, DESIGN.md. There is no page for it here, so
    # point at the source rather than leaving a link that 404s.
    return f"{REPO}/blob/main/{path}{anchor}"


def main() -> int:
    src, _slug, title, blurb, toc = sys.argv[1:6]

    with open(src, encoding="utf8") as handle:
        body = handle.read()

    # The layout emits the title itself, so a leading `# Heading` would
    # show it twice.
    lines = body.split("\n")
    if lines and lines[0].startswith("# "):
        lines = lines[1:]
        while lines and not lines[0].strip():
            lines = lines[1:]
    body = "\n".join(lines)

    # Rewrite links outside fenced code blocks. Inside a fence a `](...)`
    # is example text, not a link, and Hugo will not render it as one.
    out, fenced = [], False
    for line in body.split("\n"):
        if line.lstrip().startswith("```"):
            fenced = not fenced
        if not fenced:
            line = LINK.sub(lambda m: "](" + rewrite(m.group(1)) + ")", line)
        out.append(line)

    print("---")
    print(f'title: "{title}"')
    print(f'blurb: "{blurb}"')
    print(f"toc: {toc}")
    print(f'sourcefile: "{src}"')
    print("---")
    print()
    print("\n".join(out).rstrip())
    return 0


if __name__ == "__main__":
    sys.exit(main())
