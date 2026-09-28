const copyStatus = document.querySelector(".copy-status");

const COPY_ICON =
  '<svg class="copy-icon" viewBox="0 0 24 24" aria-hidden="true"><rect x="9" y="9" width="11" height="11" rx="2" fill="none" stroke="currentColor" stroke-width="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" fill="none" stroke="currentColor" stroke-linecap="round" stroke-width="2"/></svg>' +
  '<svg class="check-icon" viewBox="0 0 24 24" aria-hidden="true"><path d="M20 6 9 17l-5-5" fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="2.4"/></svg>';

function wireCopy(button, getText) {
  const title = button.dataset.copyTitle ?? "Copy";
  const done = button.dataset.copyLabel ?? "Copied.";

  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(getText());
      if (copyStatus) copyStatus.textContent = done;
      button.classList.add("is-copied");
      button.setAttribute("aria-label", done);
      button.setAttribute("title", "Copied");
      window.setTimeout(() => {
        button.classList.remove("is-copied");
        button.setAttribute("aria-label", title);
        button.setAttribute("title", title);
      }, 1800);
    } catch {
      if (copyStatus) copyStatus.textContent = "Copy failed.";
    }
  });
}

// Buttons with an explicit value (the install command).
document.querySelectorAll("[data-copy]").forEach((button) => {
  wireCopy(button, () => button.dataset.copy ?? "");
});

// Every code panel gets a copy button. Prompt markers and comments are
// visual only, so they are left out of the copied text.
document.querySelectorAll(".code-panel").forEach((panel) => {
  const code = panel.querySelector("code");
  if (!code || panel.dataset.nocopy !== undefined) return;

  const button = document.createElement("button");
  button.type = "button";
  button.className = "copy-button";
  button.dataset.copyTitle = "Copy code";
  button.dataset.copyLabel = "Code copied.";
  button.setAttribute("aria-label", "Copy code");
  button.setAttribute("title", "Copy code");
  button.innerHTML = COPY_ICON;
  panel.append(button);

  wireCopy(button, () => {
    const clone = code.cloneNode(true);
    clone.querySelectorAll(".prompt, .comment").forEach((node) => node.remove());
    return clone.textContent
      .split("\n")
      .map((line) => line.trimEnd())
      .join("\n")
      .trim();
  });
});

// Tabs: arrow keys move between tabs, as in the WAI-ARIA tabs pattern.
document.querySelectorAll("[data-tabs]").forEach((root) => {
  const tabs = [...root.querySelectorAll('[role="tab"]')];

  const select = (tab) => {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute("aria-selected", String(on));
      t.tabIndex = on ? 0 : -1;
      document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
    });
  };

  tabs.forEach((tab, i) => {
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (event) => {
      const step = { ArrowRight: 1, ArrowLeft: -1 }[event.key];
      if (!step) return;
      event.preventDefault();
      const next = tabs[(i + step + tabs.length) % tabs.length];
      select(next);
      next.focus();
    });
  });
});

// Doc pages: highlight the section being read in the "On this page" index.
const navLinks = [...document.querySelectorAll(".doc-nav a[href^='#']")];
if (navLinks.length && "IntersectionObserver" in window) {
  const byId = new Map(navLinks.map((a) => [a.getAttribute("href").slice(1), a]));
  const visible = new Set();

  const observer = new IntersectionObserver(
    (entries) => {
      entries.forEach((e) => (e.isIntersecting ? visible.add(e.target.id) : visible.delete(e.target.id)));
      const current = [...byId.keys()].find((id) => visible.has(id));
      if (!current) return;
      navLinks.forEach((a) => a.classList.toggle("is-active", a === byId.get(current)));
    },
    { rootMargin: "-80px 0px -60% 0px" },
  );

  byId.forEach((_, id) => {
    const section = document.getElementById(id);
    if (section) observer.observe(section);
  });
}
