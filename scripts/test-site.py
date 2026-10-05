#!/usr/bin/env python3
"""Check static-site metadata, structured data, sitemap and local links."""

import json
import re
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
ORIGIN = "https://agentpc.pawanpaudel.com.np"
PAGES = {"index.html": "/", "mcp.html": "/mcp", "cli.html": "/cli", "guide.html": "/guide"}


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        self.meta = {}
        self.canonicals = []
        self.links = []
        self.ids = []
        self.titles = []
        self.headings = []
        self.schemas = []
        self.capture = None
        self.buffer = []
        self.feed(text)
        self.close()

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "id" in attrs:
            self.ids.append(attrs["id"])
        if tag == "meta":
            key = attrs.get("name", attrs.get("property"))
            self.meta.setdefault(key, []).append(attrs.get("content", ""))
        if tag == "link" and attrs.get("rel") == "canonical":
            self.canonicals.append(attrs.get("href"))
        if tag in ("a", "link") and "href" in attrs:
            self.links.append(attrs["href"])
        if tag in ("img", "script") and "src" in attrs:
            self.links.append(attrs["src"])
        if tag in ("title", "h1") or (tag == "script" and attrs.get("type") == "application/ld+json"):
            self.capture = tag
            self.buffer = []

    def handle_data(self, data):
        if self.capture:
            self.buffer.append(data)

    def handle_endtag(self, tag):
        if tag != self.capture:
            return
        text = "".join(self.buffer).strip()
        if tag == "title":
            self.titles.append(text)
        elif tag == "h1":
            self.headings.append(text)
        elif tag == "script":
            self.schemas.append(json.loads(text))
        self.capture = None


def check():
    pages = {name: Page((SITE / name).read_text()) for name in PAGES}
    errors = []

    def require(condition, message):
        if not condition:
            errors.append(message)

    titles, descriptions = [], []
    for name, page in pages.items():
        canonical = ORIGIN + PAGES[name]
        require(len(page.titles) == 1 and bool(page.titles[0]), f"{name}: need one title")
        require(len(page.headings) == 1 and bool(page.headings[0]), f"{name}: need one H1")
        require(page.canonicals == [canonical], f"{name}: canonical must be {canonical}")
        require(len(page.ids) == len(set(page.ids)), f"{name}: duplicate element IDs")
        description = page.meta.get("description", [])
        require(len(description) == 1 and bool(description[0]), f"{name}: need one description")
        if description:
            descriptions.append(description[0])
        if page.titles:
            titles.append(page.titles[0])
        for key, expected in {
            "og:url": canonical,
            "og:title": page.titles[0] if page.titles else "",
            "og:description": description[0] if description else "",
            "og:image": ORIGIN + "/og.png",
            "twitter:card": "summary_large_image",
            "twitter:title": page.titles[0] if page.titles else "",
            "twitter:description": description[0] if description else "",
            "twitter:image": ORIGIN + "/og.png",
        }.items():
            require(page.meta.get(key) == [expected], f"{name}: missing or inconsistent {key}")
        require(len(page.schemas) == 1, f"{name}: need one JSON-LD block")
        if page.schemas:
            schema = page.schemas[0]
            require(schema.get("@context") == "https://schema.org", f"{name}: schema context")
            require("aggregateRating" not in schema and "review" not in schema, f"{name}: unsupported ratings")
            if name == "index.html":
                require(schema.get("@type") == "SoftwareSourceCode", f"{name}: software schema type")
                require(schema.get("url") == canonical, f"{name}: schema URL")
                require(schema.get("codeRepository") == "https://github.com/pawanpaudel93/agentpc", f"{name}: repository")
                require(schema.get("license") == "https://opensource.org/license/mit", f"{name}: license")
            else:
                require(schema.get("@type") == "BreadcrumbList", f"{name}: breadcrumb schema type")
                items = schema.get("itemListElement", [])
                require([i.get("position") for i in items] == [1, 2], f"{name}: breadcrumb positions")
                require([i.get("item") for i in items] == [ORIGIN + "/", canonical], f"{name}: breadcrumb URLs")

        for link in page.links:
            parsed = urlsplit(link)
            if parsed.scheme or parsed.netloc:
                if parsed.netloc != urlsplit(ORIGIN).netloc:
                    continue
            route = unquote(parsed.path)
            require(not route.endswith(".html"), f"{name}: noncanonical link {link}")
            if not route:
                target = SITE / name
            else:
                target = SITE / route.lstrip("/")
                if route == "/":
                    target = SITE / "index.html"
                elif not target.suffix:
                    target = target.with_suffix(".html")
            # The installer is served by a hosting redirect rather than a local file.
            if route == "/install.sh":
                continue
            require(target.is_file(), f"{name}: broken local link {link}")
            if parsed.fragment and target.name in pages:
                require(unquote(parsed.fragment) in pages[target.name].ids, f"{name}: missing fragment {link}")

    require(len(set(titles)) == len(PAGES), "page titles must be unique")
    require(len(set(descriptions)) == len(PAGES), "page descriptions must be unique")
    sitemap = ET.parse(SITE / "sitemap.xml")
    urls = [e.text for e in sitemap.findall(".//{http://www.sitemaps.org/schemas/sitemap/0.9}loc")]
    require(sorted(urls) == sorted(ORIGIN + route for route in PAGES.values()), "sitemap must list every canonical once")
    require(f"Sitemap: {ORIGIN}/sitemap.xml" in (SITE / "robots.txt").read_text(), "robots.txt must advertise sitemap")
    require(".html" not in (SITE / "llms.txt").read_text(), "llms.txt must use canonical URLs")

    # The tool count and list: the source's tools, AGENTS.md's table, the home page's number
    # and the MCP page's tables must agree.
    source = re.findall(r"^    #\[tool\(", (ROOT / "src" / "mcp.rs").read_text(), re.M)
    table = (ROOT / "AGENTS.md").read_text().split("## Using the VMs", 1)[1].split("\n## ", 1)[0]
    documented = {n for line in table.splitlines() if line.startswith("| `")
                  for n in re.findall(r"`([a-z_]+)[`(]", line.split("|")[1])}
    advertised = re.search(r"All (\d+) tools", (SITE / "index.html").read_text())
    require(len(source) == len(documented), f"src/mcp.rs has {len(source)} tools, AGENTS.md lists {len(documented)}")
    require(advertised is not None and int(advertised.group(1)) == len(documented),
            f"index.html must say 'All {len(documented)} tools'")
    mcp_page = (SITE / "mcp.html").read_text()
    for tool in sorted(documented):
        require(f'translate="no">{tool}</code>' in mcp_page, f"mcp.html must document {tool}")
    # The recovery/readiness vocabulary: the new states, reasons and error codes must be in the
    # source (so a typo here is caught) and in every document an agent or user reads.
    vocabulary = ["list_jobs", "wait_for_service", "request_cancelled", "cursor_stale",
                  "scan_truncated", "job_failed", "probe_unsupported", "status_matched",
                  "timeout_seconds", "notifications/cancelled"]
    source_text = "".join((ROOT / "src" / f).read_text() for f in ("mcp.rs", "jobs.rs", "readiness.rs", "execution.rs"))
    documents = {
        "AGENTS.md": (ROOT / "AGENTS.md").read_text(),
        "README.md": (ROOT / "README.md").read_text(),
        "plugin/skills/agentpc/SKILL.md": (ROOT / "plugin" / "skills" / "agentpc" / "SKILL.md").read_text(),
        "site/mcp.html": mcp_page,
    }
    for term in vocabulary:
        if term != "notifications/cancelled":  # the protocol's own method name, not in our source
            camel = "".join(word.title() for word in term.split("_"))  # serde snake_case of a Rust variant
            require(term in source_text or camel in source_text,
                    f"the source no longer has {term}: update the vocabulary list")
        for doc, text in documents.items():
            require(term in text, f"{doc} must document {term}")
    require("list_jobs" in (SITE / "llms.txt").read_text() and "wait_for_service" in (SITE / "llms.txt").read_text(),
            "llms.txt must mention list_jobs and wait_for_service")
    if errors:
        for error in errors:
            print(f"FAIL: {error}")
        raise SystemExit(1)
    print(f"PASS: {len(pages)} pages — metadata, schemas, sitemap, local links and fragments")


if __name__ == "__main__":
    check()
