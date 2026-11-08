#!/usr/bin/env python3
"""Build the CPS manual as a static website."""

import argparse
from dataclasses import dataclass
from html import escape
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import shutil
from string import Template
from urllib.parse import quote, unquote, urlsplit, urlunsplit

from markdown import Markdown
from markdown.treeprocessors import Treeprocessor


DOCS = Path(__file__).resolve().parents[1]
ROOT = DOCS.parent
ASSETS = Path(__file__).resolve().parent
REPOSITORY = "https://github.com/kevinard/cps"
GROUPS = {
    "Start here": ("", ["quickstart.md", "concepts.md"]),
    "Setup guides": ("setups", None),
    "Clients": ("client", None),
    "Package maintainers": ("maintainer", None),
    "Publishers": ("publisher", None),
    "Developers": ("developer", None),
    "Reference": ("reference", None),
}


def relative_link(source, destination):
    return quote(os.path.relpath(destination, source.parent), safe="/")


def github_slug(title, separator):
    return "".join(
        separator if char == " " else char
        for char in title.lower()
        if char.isalnum() or char in " _-"
    )


@dataclass
class Page:
    title: str
    html: str
    headings: list
    search_entries: list


class SearchSections(HTMLParser):
    """Index the rendered text, including fenced commands and configuration."""

    def __init__(self):
        super().__init__()
        self.sections = [([], "", [])]
        self.in_heading = False

    def handle_starttag(self, tag, attrs):
        if tag in ("h2", "h3", "h4"):
            self.sections.append(([], dict(attrs).get("id", ""), []))
            self.in_heading = True

    def handle_endtag(self, tag):
        if tag in ("h2", "h3", "h4"):
            self.in_heading = False

    def handle_data(self, data):
        heading, _, body = self.sections[-1]
        (heading if self.in_heading else body).append(data)

    def entries(self):
        for heading, fragment, body in self.sections:
            yield "".join(heading), fragment, " ".join(" ".join(body).split())


class SiteLinks(Treeprocessor):
    def __init__(self, markdown, source, output, routes):
        super().__init__(markdown)
        self.source = source
        self.output = output
        self.routes = routes
        self.internal_links = []
        self.title = ""
        self.anchors = set()

    def run(self, root):
        heading = root.find("h1")
        if heading is None:
            raise ValueError(f"{self.source}: expected an H1")
        self.title = "".join(heading.itertext())

        # The site shell supplies navigation for Markdown readers' breadcrumbs.
        children = list(root)
        if len(children) > 1 and children[1].tag == "p":
            first_link = children[1].find("a")
            if first_link is not None and first_link.text in ("Documentation", "Project README"):
                root.remove(children[1])
        for heading, listing in zip(children, children[1:]):
            if heading.tag == "h2" and heading.text == "On this page" and listing.tag == "ul":
                root.remove(heading)
                root.remove(listing)
                toc = self.md.toc_tokens[0]["children"]
                toc[:] = [item for item in toc if item["id"] != "on-this-page"]
                break
        self.anchors = {element.get("id") for element in root.iter() if element.get("id")}

        for link in root.iter("a"):
            href = link.get("href", "")
            url = urlsplit(href)
            if url.scheme or url.netloc:
                continue
            target = (self.source.parent / unquote(url.path)).resolve() if url.path else self.source
            if target in self.routes:
                destination = self.routes[target]
                self.internal_links.append((destination, unquote(url.fragment)))
                path = relative_link(self.output, destination) if url.path else ""
            else:
                if not target.exists() or not target.is_relative_to(ROOT):
                    raise ValueError(f"{self.source.relative_to(ROOT)}: unresolved link {href}")
                kind = "tree" if target.is_dir() else "blob"
                path = f"{REPOSITORY}/{kind}/main/{quote(target.relative_to(ROOT).as_posix())}"
            link.set("href", urlunsplit(url._replace(path=path)))
        return root


def headings_html(items):
    return "<ul>" + "".join(
        f'<li><a href="#{escape(item["id"])}">{item["name"]}</a>'
        f'{headings_html(item["children"]) if item["children"] else ""}</li>'
        for item in items
    ) + "</ul>"


def navigation(output, pages, routes):
    groups = []
    for label, (directory, files) in GROUPS.items():
        if files is None:
            contents = DOCS / directory
            chapters = sorted(path for path in contents.glob("*.md") if path.name != "README.md")
            chapters.insert(0, contents / "README.md")
        else:
            chapters = [DOCS / name for name in files]
        links = []
        for path in chapters:
            destination = routes[path]
            active = ' aria-current="page"' if destination == output else ""
            title = "Overview" if path.name == "README.md" else pages[path].title
            links.append(f'<li><a href="{relative_link(output, destination)}"{active}>{escape(title)}</a></li>')
        active_group = output in (routes[path] for path in chapters)
        expanded = " open" if active_group or (output == Path("index.html") and not directory) else ""
        groups.append(f"<details{expanded}><summary>{label}</summary><ul>{''.join(links)}</ul></details>")
    return "\n".join(groups)


def build(destination):
    sources = sorted(DOCS.rglob("*.md"))
    routes = {
        path: path.relative_to(DOCS).with_name(
            "index.html" if path.name == "README.md" else f"{path.stem}.html"
        )
        for path in sources
    }
    man_pages = sorted((DOCS / "reference" / "man").glob("*.1"))
    routes.update({path: path.relative_to(DOCS).with_suffix(".1.txt") for path in man_pages})
    pages, links, anchors = {}, {}, {}
    for source in sources:
        output = routes[source]
        md = Markdown(
            extensions=["fenced_code", "tables", "toc"],
            extension_configs={"toc": {"slugify": github_slug}},
        )
        reader = SiteLinks(md, source, output, routes)
        md.treeprocessors.register(reader, "site-links", 0)
        html = md.convert(source.read_text(encoding="utf-8"))
        search_sections = SearchSections()
        search_sections.feed(html)
        search_sections.close()
        pages[source] = Page(
            reader.title, html, md.toc_tokens[0]["children"], list(search_sections.entries())
        )
        links[source] = reader.internal_links
        anchors[output] = reader.anchors
    for source, targets in links.items():
        for target, fragment in targets:
            if fragment and fragment not in anchors.get(target, ()):
                raise ValueError(f"{source.relative_to(ROOT)}: unknown anchor {target}#{fragment}")

    destination.mkdir(parents=True, exist_ok=True)
    template = Template((ASSETS / "page.html").read_text(encoding="utf-8"))
    search = []
    for source, page in pages.items():
        output = routes[source]
        current = output.as_posix()
        outline = headings_html(page.headings) if page.headings else ""
        section = next(
            (name for name, (directory, _) in GROUPS.items() if directory and directory == output.parts[0]),
            "Start here",
        )
        values = {
            "title": escape(page.title),
            "section": escape(section),
            "home": relative_link(output, Path("index.html")),
            "style": relative_link(output, Path("site.css")),
            "script": relative_link(output, Path("site.js")),
            "index": relative_link(output, Path("search-index.js")),
            "navigation": navigation(output, pages, routes),
            "outline": outline,
            "content": page.html,
            "repository": REPOSITORY,
        }
        target = destination / output
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(template.substitute(values), encoding="utf-8")
        for heading, fragment, body in page.search_entries:
            search.append([page.title, heading, current + (f"#{fragment}" if fragment else ""), body])
    for path in man_pages:
        target = destination / routes[path]
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, target)
    for name in ("site.css", "site.js"):
        shutil.copyfile(ASSETS / name, destination / name)
    (destination / "search-index.js").write_text(
        f"window.cpsSearch = {json.dumps(search, separators=(',', ':'))};\n",
        encoding="utf-8",
    )
    print(f"Built {len(pages)} pages in {destination}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target" / "docs")
    build(parser.parse_args().output.resolve())
