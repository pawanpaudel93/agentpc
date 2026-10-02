# agentpc Site

Static site for `https://agentpc.pawanpaudel.com.np`.

Deploy the `site/` directory as the web root. The site is plain HTML/CSS/JS and
does not require a build step.

Pages:

- `index.html` — product page and installer.
- `mcp.html` — MCP setup and tool reference.
- `cli.html` — command-line reference.
- `guide.html` — images, networking, configuration, troubleshooting, security.

Shared assets:

- `styles.css` — site styling.
- `script.js` — copy buttons, tabs, and the doc-page section highlight.
- `favicon.svg` — the agentpc mark (a copy of `assets/logo.svg`; keep them identical).
- `apple-touch-icon.png`, `og.png` — the mark as a 180px icon and a 1200×630 social card.

## Search and Canonical URLs

Canonical URLs are `/`, `/mcp`, `/cli` and `/guide`, without a trailing slash
on documentation pages. HTML canonical links, Open Graph URLs, the sitemap,
`llms.txt` and internal navigation all use these routes. The physical files
remain `index.html`, `mcp.html`, `cli.html` and `guide.html`.

Vercel's `cleanUrls: true` serves these routes and permanently redirects the
`.html` URLs; `trailingSlash: false` keeps documentation URLs consistent. If
moving to another host, configure equivalent extensionless routes and redirects
before deploying. A plain file server does not provide that routing by itself.

The homepage uses `SoftwareSourceCode` JSON-LD for the public repository, MIT
license and runtime platform. Documentation pages use `BreadcrumbList`. These
are factual descriptions, not a promise of search rich results. Do not add
ratings, reviews or features that are not present in the visible content.

Run the source checks with:

```sh
python3 scripts/test-site.py
```

`scripts/check.sh` also runs them in CI. They check unique metadata, canonical
and social URL agreement, JSON-LD, sitemap coverage, local assets and fragment
links. They do not measure rankings, index coverage or browser performance.

After deploying:

- Verify `/`, `/mcp`, `/cli`, `/guide`, `/robots.txt` and `/sitemap.xml` return
  HTTP 200 without redirects or an `X-Robots-Tag: noindex` header.
- Verify `/index.html` redirects to `/`, and documentation `.html` URLs redirect
  to their extensionless canonicals.
- Inspect the rendered canonical and structured data with Google Search
  Console's URL Inspection tool; submit `/sitemap.xml` there.
- Review indexing and search queries in Search Console, and mobile performance
  in PageSpeed Insights before choosing additional content work.

## Install Redirect

`/install.sh` should return a temporary redirect to:

```text
https://raw.githubusercontent.com/pawanpaudel93/agentpc/main/install.sh
```

Included configs:

- `_redirects` for Netlify and Cloudflare Pages.
- `vercel.json` for Vercel.

The public install command is:

```sh
curl -fsSL https://agentpc.pawanpaudel.com.np/install.sh | sh
```
