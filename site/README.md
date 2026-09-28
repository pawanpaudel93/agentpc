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
