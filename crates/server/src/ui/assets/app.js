"use strict";

// Fact and page text is untrusted. Everything reaches the page through h(), which only ever sets text
// nodes and attributes, so a planted statement cannot become markup. Page bodies are markdown, and they
// are shown as plain text, never rendered.

const SVG_NS = "http://www.w3.org/2000/svg";
const GRAPH_LIMIT = 300;
const PAGE = 100;
const REFRESH_MS = 30000;
const PAGE_LIST_LIMIT = 500;
const DIFF_CONTEXT = 3;
const DIFF_MAX_CELLS = 4000000;
const VERSION_CONFLICT = "The page changed since this edit; reject it and ask for a new edit.";

const S = {
  token: "",
  scopes: [],
  truncated: false,
  scope: null,
  tab: "facts",
  health: null,
  offline: null,
  factState: "current",
  filter: "",
  facts: [],
  factsDone: false,
  graph: null,
  timeline: null,
  review: null,
  reviewDenied: false,
  pageReview: null,
  pageConflicts: new Set(),
  pendingPages: new Map(),
  editing: null,
  pageView: "rendered",
  newPage: null,
  newSpace: null,
  pages: null,
  pageFilter: "",
  page: null,
  notes: null,
  ask: null,
  drawer: null,
  selectedNode: null,
  signature: "",
};

// ---- Small helpers ----------------------------------------------------------------------------

function h(tag, props, ...children) {
  const el = document.createElement(tag);
  applyProps(el, props);
  append(el, children);
  return el;
}

function s(tag, props, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  applyProps(el, props);
  append(el, children);
  return el;
}

function applyProps(el, props) {
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "class") el.setAttribute("class", value);
    else if (key === "css") for (const [p, v] of Object.entries(value)) el.style.setProperty(p, v);
    else if (key === "value") el.value = value;
    else el.setAttribute(key, value === true ? "true" : String(value));
  }
}

function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
}

function icon(path) {
  return s("svg", { viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": 2, "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" },
    path.map((d) => s("path", { d })));
}

const ICONS = {
  search: ["M21 21l-4.3-4.3", "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14z"],
  refresh: ["M21 12a9 9 0 1 1-2.6-6.4L21 8", "M21 3v5h-5"],
  theme: ["M12 3a9 9 0 1 0 9 9 7 7 0 0 1-9-9z"],
  close: ["M18 6 6 18", "M6 6l12 12"],
};

function storageGet(store, key) {
  try { return store.getItem(key); } catch { return null; }
}

function storageSet(store, key, value) {
  try { store.setItem(key, value); } catch { /* the page works without it */ }
}

function toast(message, bad) {
  const el = h("div", { class: bad ? "toast bad" : "toast" }, message);
  document.getElementById("toasts").append(el);
  setTimeout(() => el.remove(), bad ? 6000 : 3000);
}

const dateFormat = new Intl.DateTimeFormat(undefined, { year: "numeric", month: "short", day: "numeric" });

function date(text) {
  return text ? dateFormat.format(new Date(text)) : "";
}

function ago(text) {
  if (!text) return "";
  const seconds = (Date.now() - new Date(text).getTime()) / 1000;
  const units = [[31536000, "year"], [2592000, "month"], [86400, "day"], [3600, "hour"], [60, "minute"]];
  for (const [size, name] of units) {
    if (Math.abs(seconds) >= size) {
      const n = Math.round(Math.abs(seconds) / size);
      const word = `${n} ${name}${n === 1 ? "" : "s"}`;
      return seconds >= 0 ? `${word} ago` : `in ${word}`;
    }
  }
  return "just now";
}

const humanize = (text) => String(text || "").replace(/[_-]+/g, " ");

const TRUST = {
  system: ["System", "Set by the system. The highest level of trust."],
  user: ["Confirmed", "Came from a person, or a person approved it."],
  agent: ["Agent", "Your coding agent decided to remember this. Its model also reads outside content, so writes about people and shared knowledge wait for review."],
  external: ["Outside source", "Came from outside content. The lowest level of trust."],
};

function factStatus(fact) {
  const now = Date.now();
  if (fact.status === "quarantined") return { key: "waiting", label: "Waiting for review", kind: "waiting" };
  if (fact.status === "rejected") return { key: "rejected", label: "Rejected", kind: "bad" };
  if (fact.invalidated_at && new Date(fact.invalidated_at).getTime() <= now) return { key: "retracted", label: "Retracted", kind: "bad" };
  if (new Date(fact.valid_from).getTime() > now) return { key: "later", label: "Starts later", kind: "past" };
  if (fact.valid_to && new Date(fact.valid_to).getTime() <= now) {
    return { key: "past", label: fact.superseded_by ? "Replaced" : "Ended", kind: "past" };
  }
  if (fact.expires_at && new Date(fact.expires_at).getTime() <= now) return { key: "past", label: "Expired", kind: "past" };
  return { key: "current", label: "True now", kind: "current" };
}

function pill(fact) {
  const status = factStatus(fact);
  return h("span", { class: `pill ${status.kind}` }, status.label);
}

function scopeParts(scope) {
  const at = scope.indexOf(":");
  return { kind: scope.slice(0, at), name: scope.slice(at + 1) };
}

const current = () => S.scopes.find((item) => item.scope === S.scope) || null;

// Page versions waiting for review, which the server counts apart from facts.
const pendingPagesIn = (scope) => S.pendingPages.get(scope) || 0;
const waitingIn = (summary) => summary.quarantined + pendingPagesIn(summary.scope);
const pagePath = (slug) => `pages/${encodeURIComponent(S.scope)}/${encodeURIComponent(slug)}`;

// ---- API --------------------------------------------------------------------------------------

async function api(path, options) {
  let response;
  try {
    response = await fetch(`/api/${path}`, {
      method: options?.method || "GET",
      headers: { "X-MemoryOS-UI": S.token, "Content-Type": "application/json" },
      body: options?.body ? JSON.stringify(options.body) : undefined,
    });
  } catch {
    throw Object.assign(new Error("This page lost its connection. Is `memoryos ui` still running?"), { status: 0 });
  }
  const text = await response.text();
  let body = null;
  try { body = text ? JSON.parse(text) : null; } catch { /* reported below */ }
  if (!response.ok) {
    throw Object.assign(new Error(body?.error?.message || `Request failed with status ${response.status}`), {
      status: response.status,
      code: body?.error?.code,
      body,
    });
  }
  return body;
}

const scopeQuery = (extra) => new URLSearchParams({ scope: S.scope, ...extra }).toString();

// `memoryos setup` keeps an image it already built, so a newer command can meet an older server.
async function loadScopes() {
  const [body, pending] = await Promise.all([
    api("scopes").catch((error) => {
      if (error.status !== 404) throw error;
      throw Object.assign(new Error("Your server is older than this page. Update it with: docker compose -f ~/.config/memoryos/docker-compose.yml up -d --build"), { status: 0 });
    }),
    loadPendingPages(),
  ]);
  S.scopes = body.scopes;
  S.truncated = body.truncated;
  S.pendingPages = pending;
  // A scope whose only page waits for its first review is not in the scope list. Show it anyway.
  for (const scope of pending.keys()) {
    if (!S.scopes.some((item) => item.scope === scope)) {
      S.scopes.push({ scope, current: 0, past: 0, quarantined: 0, entities: 0, blocks: 0, pages: 0 });
    }
  }
  if (!S.scope || !current()) S.scope = S.scopes[0]?.scope ?? null;
}

// Counts per scope. Empty for a key that cannot review and for a server from before pages.
async function loadPendingPages() {
  const counts = new Map();
  try {
    const body = await api(`pages/quarantine?${new URLSearchParams({ scopes: "*", limit: 200 })}`);
    for (const version of body.versions) counts.set(version.scope, (counts.get(version.scope) || 0) + 1);
  } catch {
    // The fact counts still show. The review list itself reports real failures.
  }
  return counts;
}

async function loadHealth() {
  try {
    const response = await fetch("/api/status", { headers: { "X-MemoryOS-UI": S.token } });
    S.health = await response.json();
  } catch {
    S.health = null;
  }
}

async function loadFacts(reset) {
  const params = { state: S.factState, limit: PAGE };
  if (!reset && S.facts.length) params.before = S.facts[S.facts.length - 1].id;
  const body = await api(`facts?${scopeQuery(params)}`);
  S.facts = reset ? body.facts : S.facts.concat(body.facts);
  S.factsDone = body.facts.length < PAGE;
}

async function loadTab() {
  if (!S.scope) return;
  if (S.tab === "facts") await loadFacts(true);
  if (S.tab === "map") {
    const body = await api(`facts?${scopeQuery({ state: "current", limit: GRAPH_LIMIT })}`);
    S.graph = buildGraph(body.facts);
  }
  if (S.tab === "timeline") {
    S.timeline = (await api(`facts?${scopeQuery({ state: "all", limit: GRAPH_LIMIT })}`)).facts;
  }
  if (S.tab === "review") await loadReview();
  if (S.tab === "pages") await loadPages();
  if (S.tab === "notes") {
    S.notes = (await api(`agents/${encodeURIComponent(scopeParts(S.scope).name)}/blocks`)).blocks;
  }
}

async function loadReview() {
  S.reviewDenied = false;
  const [facts, pages] = await Promise.all([reviewFacts(), reviewPages()]);
  S.review = facts;
  S.pageReview = pages;
}

async function reviewFacts() {
  try {
    const body = await api(`quarantine?${new URLSearchParams({ scopes: S.scope, limit: 200 })}`);
    return await Promise.all(body.facts.map(async (fact) => {
      const [replaces, source] = await Promise.all([
        fact.pending_supersede_of ? api(`facts/${fact.pending_supersede_of}`).catch(() => null) : null,
        fact.source_episode_ids[0] ? api(`episodes/${fact.source_episode_ids[0]}`).catch(() => null) : null,
      ]);
      return { fact, replaces, source };
    }));
  } catch (error) {
    if (error.status !== 403) throw error;
    S.reviewDenied = true;
    return [];
  }
}

// Each pending version with the page's current version number, so an edit made over an older
// version is flagged before anyone presses Approve.
async function reviewPages() {
  let body;
  try {
    body = await api(`pages/quarantine?${new URLSearchParams({ scopes: S.scope, limit: 200 })}`);
  } catch (error) {
    // 403: a key that cannot review, reported with the facts. 404: a server from before pages.
    if (error.status === 403 || error.status === 404) return [];
    throw error;
  }
  const slugs = [...new Set(body.versions.map((version) => version.slug))];
  const currents = new Map(await Promise.all(slugs.map(async (slug) => {
    const page = await api(pagePath(slug)).catch(() => null);
    return [slug, page ? page.version : null];
  })));
  return body.versions.map((version) => ({ version, current: currents.get(version.slug) ?? null }));
}

async function loadPages() {
  const body = await api(`pages?${scopeQuery({ limit: PAGE_LIST_LIMIT })}`);
  S.pages = body.pages;
  if (S.page) await loadPage(S.page.slug, S.page.version);
}

async function loadPage(slug, version) {
  const path = pagePath(slug);
  const [page, versions] = await Promise.all([
    api(version ? `${path}?${new URLSearchParams({ version })}` : path),
    api(`${path}/versions`),
  ]);
  if (S.page?.slug !== slug || S.page.version !== version) return;
  S.page.data = page;
  S.page.versions = versions.versions;
}

// ---- Actions ----------------------------------------------------------------------------------

function guarded(work) {
  return work().catch((error) => {
    if (error.status === 0 || error.code === "server_unreachable" || error.status === 401) {
      S.offline = error.message;
      render();
    } else {
      toast(error.message, true);
    }
  });
}

function selectScope(scope) {
  S.scope = scope;
  S.drawer = null;
  S.ask = null;
  S.filter = "";
  S.selectedNode = null;
  S.facts = [];
  S.graph = S.timeline = S.review = S.notes = null;
  S.pageReview = S.pages = S.page = null;
  S.pageFilter = "";
  const summary = current();
  if (S.tab === "notes" && !summary?.blocks) S.tab = "facts";
  render();
  return guarded(async () => { await loadTab(); S.offline = null; render(); });
}

function selectTab(tab) {
  if (tab === "pages" && S.tab === "pages") S.page = null;
  S.tab = tab;
  S.ask = null;
  render();
  return guarded(async () => { await loadTab(); render(); });
}

function ask(query) {
  const text = query.trim();
  if (!text) { S.ask = null; render(); return Promise.resolve(); }
  S.ask = { query: text, results: null };
  render();
  return guarded(async () => {
    const body = await api("recall", { method: "POST", body: { scopes: [S.scope], query: text, limit: 10 } });
    if (S.ask?.query === text) { S.ask.results = body.facts; render(); }
  });
}

async function openFact(id) {
  S.drawer = { type: "fact", id, fact: null, history: null, sources: null };
  renderDrawer();
  await guarded(async () => {
    const [fact, history] = await Promise.all([api(`facts/${id}`), api(`facts/${id}/history`)]);
    const sources = await Promise.all(fact.source_episode_ids.slice(0, 3).map((e) => api(`episodes/${e}`).catch(() => null)));
    if (S.drawer?.id !== id) return;
    S.drawer.fact = fact;
    S.drawer.history = [...history.facts].sort((a, b) => new Date(a.valid_from) - new Date(b.valid_from));
    S.drawer.sources = sources.filter(Boolean);
    renderDrawer();
  });
}

function closeDrawer() {
  S.drawer = null;
  S.selectedNode = null;
  render();
}

function openPage(slug, version) {
  S.page = { slug, version: version || null, data: null, versions: null };
  S.tab = "pages";
  S.ask = null;
  render();
  document.getElementById("body")?.scrollTo(0, 0);
  return guarded(async () => {
    if (S.pages === null) S.pages = (await api(`pages?${scopeQuery({ limit: PAGE_LIST_LIMIT })}`)).pages;
    await loadPage(slug, version || null);
    render();
  });
}

// "Open in editor": the local UI server opens the page in the editor `memoryos ui` was started
// with, and saves a new version when it closes. This polls until then.
async function editInEditor(slug, scope = S.scope) {
  const query = new URLSearchParams({ scope, slug });
  const local = async (method) => {
    const response = await fetch(method === "POST" ? "/local/edit" : `/local/edit?${query}`, {
      method,
      headers: { "X-MemoryOS-UI": S.token, "Content-Type": "application/json" },
      body: method === "POST" ? JSON.stringify({ scope, slug }) : undefined,
    });
    const body = await response.json().catch(() => null);
    if (!response.ok) throw new Error(body?.error?.message || `Request failed with status ${response.status}`);
    return body;
  };
  try {
    const started = await local("POST");
    S.editing = slug;
    render();
    toast(`Opened in ${started.editor}. Save and close it to make a new version.`);
    for (;;) {
      await new Promise((resolve) => setTimeout(resolve, 1500));
      const status = await local("GET");
      if (status.editing) continue;
      if (status.error) toast(status.error, true);
      else if (status.saved) toast(status.saved);
      break;
    }
  } catch (error) {
    toast(error.message, true);
  }
  if (S.editing === slug) S.editing = null;
  if (scope !== S.scope) {
    // A new space exists once its first page is saved. Show it, or say why it is not there.
    await guarded(async () => {
      await loadScopes();
      if (S.scopes.some((item) => item.scope === scope)) {
        S.tab = "pages";
        await selectScope(scope);
      } else {
        toast(`${scope} was not created: nothing was saved`, true);
        render();
      }
    });
    return;
  }
  await guarded(async () => { S.pendingPages = await loadPendingPages(); await loadPages(); render(); });
}

function closePage() {
  S.page = null;
  render();
  return guarded(async () => { await loadPages(); render(); });
}

const pendingKey = (version) => `${version.page_id}/${version.version}`;

async function decidePage(version, action) {
  const key = pendingKey(version);
  await guarded(async () => {
    try {
      await api(`pages/quarantine/${version.page_id}/${version.version}/${action}`, { method: "POST" });
    } catch (error) {
      if (error.code !== "version_conflict") throw error;
      S.pageConflicts.add(key);
      toast(VERSION_CONFLICT, true);
      return;
    }
    S.pageConflicts.delete(key);
    toast(action === "approve" ? "Approved. This is now the page your agent reads." : "Rejected. The page stays as it was.");
    if (S.page?.slug === version.slug) S.page = null;
    S.pages = null;
    await Promise.all([loadScopes(), loadReview()]);
  });
  // Also after a failure, so the buttons are enabled again.
  render();
}

async function decide(id, action) {
  await guarded(async () => {
    await api(`quarantine/${id}/${action}`, { method: "POST" });
    toast(action === "approve" ? "Approved. Your agent can now recall this." : "Rejected. It will not be recalled.");
    if (S.drawer?.id === id) S.drawer = null;
    await Promise.all([loadScopes(), loadReview()]);
  });
  // Also after a failure, so the buttons are enabled again.
  render();
}

async function refresh(quiet) {
  await guarded(async () => {
    await Promise.all([loadScopes(), loadHealth()]);
    const recovered = Boolean(S.offline);
    S.offline = null;
    const signature = JSON.stringify([S.scopes, [...S.pendingPages]]);
    const changed = recovered || signature !== S.signature;
    S.signature = signature;
    if (!quiet || changed) {
      await loadTab();
      render();
    } else {
      renderRail();
    }
    if (!quiet) toast("Up to date");
  });
}

// ---- Layout -----------------------------------------------------------------------------------

function render() {
  const app = document.getElementById("app");
  if (!S.token) {
    app.replaceChildren(h("div", { class: "empty" },
      h("h2", {}, "This page needs its address"),
      h("p", {}, "Run ", h("code", {}, "memoryos ui"), " and open the full address it prints.")));
    return;
  }
  app.replaceChildren(h("div", { class: "shell" },
    h("aside", { class: "rail", id: "rail" }),
    h("main", { class: "main" },
      S.offline && h("div", { class: "banner", role: "alert" }, S.offline),
      renderHead(),
      h("div", { class: `body${S.tab === "map" && !S.ask && S.scope ? " flush" : ""}`, id: "body" }))));
  renderRail();
  renderBody();
  renderDrawer();
}

function renderRail() {
  const rail = document.getElementById("rail");
  if (!rail) return;
  const groups = [["user", "People"], ["agent", "Agents"], ["shared", "Shared"]];
  rail.replaceChildren();
  append(rail, [
    h("div", { class: "brand" },
      h("img", { class: "brand-mark", alt: "", src: document.querySelector("link[rel=icon]").href }), "MemoryOS"),
    groups.map(([kind, title]) => {
      const items = S.scopes.filter((item) => scopeParts(item.scope).kind === kind);
      if (kind === "shared") return h("div", { class: "rail-group" }, h("h2", {}, title), items.map(renderSpace), renderNewSpace());
      if (!items.length) return null;
      return h("div", { class: "rail-group" }, h("h2", {}, title), items.map(renderSpace));
    }),
    S.truncated && h("p", { class: "help" }, "Showing the 500 most recently used spaces."),
    h("div", { class: "rail-foot" }, healthDot(), h("span", {}, healthLabel()), h("span", { class: "spacer" }),
      h("button", { class: "icon-btn", title: "Refresh", "aria-label": "Refresh", onclick: () => refresh(false) }, icon(ICONS.refresh)),
      h("button", { class: "icon-btn", title: "Switch light and dark", "aria-label": "Switch light and dark", onclick: toggleTheme }, icon(ICONS.theme))),
  ]);
}

// "+ New space": a shared space and its first page. The space exists once that page is saved.
function renderNewSpace() {
  if (S.newSpace === null) {
    return h("button", { class: "space new-space", onclick: () => { S.newSpace = { name: "", page: "" }; renderRail(); document.getElementById("new-space-name")?.focus(); } },
      h("span", { class: "space-avatar" }, "+"), h("span", { class: "space-name" }, "New space"));
  }
  const create = () => {
    const name = S.newSpace.name.trim();
    const page = S.newSpace.page.trim();
    if (!/^[A-Za-z0-9_.-]{1,128}$/.test(name)) {
      toast("Name the space with letters, digits, '.', '_' and '-', such as ops", true);
      return;
    }
    if (!/^[a-z0-9][a-z0-9._-]{0,127}$/.test(page)) {
      toast("Name the first page with lowercase letters, digits, '.', '_' and '-', such as overview", true);
      return;
    }
    S.newSpace = null;
    renderRail();
    editInEditor(page, `shared:${name}`);
  };
  const cancel = () => { S.newSpace = null; renderRail(); };
  const keys = (event) => { if (event.key === "Enter") create(); if (event.key === "Escape") cancel(); };
  return h("div", { class: "new-space-form" },
    h("input", { id: "new-space-name", class: "filter", placeholder: "space name, e.g. ops", "aria-label": "Space name", value: S.newSpace.name,
      oninput: (event) => { S.newSpace.name = event.target.value; }, onkeydown: keys }),
    h("input", { class: "filter", placeholder: "first page, e.g. overview", "aria-label": "First page name", value: S.newSpace.page,
      oninput: (event) => { S.newSpace.page = event.target.value; }, onkeydown: keys }),
    h("div", { class: "new-space-actions" },
      h("button", { class: "btn primary", onclick: create }, "Write first page"),
      h("button", { class: "link-btn", onclick: cancel }, "Cancel")));
}

function renderSpace(item) {
  const { name } = scopeParts(item.scope);
  return h("button", {
    class: "space", title: item.scope, "aria-current": item.scope === S.scope ? "true" : null,
    onclick: () => selectScope(item.scope),
  },
    h("span", { class: "space-avatar" }, name.slice(0, 1)),
    h("span", { class: "space-name" }, name),
    waitingIn(item) > 0 && h("span", { class: "review-dot", title: `${waitingIn(item)} waiting for review` }),
    h("span", { class: "space-count" }, item.current));
}

function healthDot() {
  const ready = S.health?.status === "ready";
  const degraded = S.health && !ready;
  return h("span", { class: `status-dot${S.offline || !S.health ? " bad" : degraded ? " warn" : ""}` });
}

function healthLabel() {
  if (S.offline || !S.health) return "Server offline";
  if (S.health.status === "ready") return "Connected";
  return S.health.database === "ok" ? "Model loading" : "Database offline";
}

function renderHead() {
  const summary = current();
  if (!summary) {
    return h("header", { class: "head" }, h("div", { class: "head-top" }, h("h1", {}, "MemoryOS")));
  }
  const { name, kind } = scopeParts(summary.scope);
  const waiting = waitingIn(summary);
  const tabs = [["facts", "Facts"], ["map", "Map"], ["timeline", "Timeline"], ["pages", "Knowledge"], ["review", "Review"]];
  if (summary.blocks > 0) tabs.push(["notes", "Agent notes"]);
  return h("header", { class: "head" },
    h("select", {
      class: "mobile-select", "aria-label": "Memory space",
      onchange: (event) => selectScope(event.target.value),
    }, S.scopes.map((item) => h("option", { value: item.scope, selected: item.scope === S.scope }, item.scope))),
    h("div", { class: "head-top" }, h("h1", {}, name), h("span", { class: "scope-code" }, `${kind} space`)),
    h("div", { class: "stats" },
      stat(summary.current, "Known now", () => showFacts("current")),
      stat(summary.past, "Replaced or ended", () => showFacts("past")),
      stat(waiting, "Waiting for review", () => selectTab("review"), waiting > 0),
      stat(summary.entities, "People and things", () => selectTab("map"))),
    h("div", { class: "ask" }, icon(ICONS.search),
      h("input", {
        id: "ask", type: "search", placeholder: "Ask what this memory knows, the way your agent would", "aria-label": "Ask this memory",
        value: S.ask?.query || "",
        onkeydown: (event) => {
          if (event.key === "Enter") ask(event.target.value);
          if (event.key === "Escape") { event.target.value = ""; ask(""); event.target.blur(); }
        },
      }),
      h("kbd", {}, "/")),
    h("nav", { class: "tabs", role: "tablist" }, tabs.map(([key, label]) => h("button", {
      class: "tab", role: "tab", "aria-selected": !S.ask && S.tab === key ? "true" : "false",
      onclick: () => selectTab(key),
    }, label,
    key === "pages" && summary.pages > 0 && h("span", { class: "tab-count" }, summary.pages),
    key === "review" && waiting > 0 && h("span", { class: "tab-badge" }, waiting)))));
}

function stat(value, label, onclick, attention) {
  return h("button", { class: `stat${attention ? " attention" : ""}`, onclick },
    h("div", { class: "stat-value" }, value.toLocaleString()),
    h("div", { class: "stat-label" }, label));
}

function showFacts(state) {
  S.factState = state;
  S.tab = "facts";
  S.ask = null;
  render();
  return guarded(async () => { await loadFacts(true); render(); });
}

function renderBody() {
  const body = document.getElementById("body");
  if (!body) return;
  if (!S.scope) { body.replaceChildren(welcome()); return; }
  if (S.ask) { body.replaceChildren(renderAsk()); return; }
  const views = { facts: renderFacts, map: renderMap, timeline: renderTimeline, pages: renderPages, review: renderReview, notes: renderNotes };
  body.replaceChildren(views[S.tab]());
}

function welcome() {
  if (S.offline) {
    return h("div", { class: "empty" }, h("h2", {}, "Cannot reach MemoryOS"),
      h("p", {}, "Start the server with ", h("code", {}, "memoryos setup"), ", then refresh this page."));
  }
  return h("div", { class: "empty" }, h("h2", {}, "Nothing remembered yet"),
    h("p", {}, "Work with your agent and ask it to remember something. It will show up here."));
}

const loading = () => h("div", { class: "loading" }, "Loading");

function empty(title, text) {
  return h("div", { class: "empty" }, h("h2", {}, title), h("p", {}, text));
}

// ---- Facts ------------------------------------------------------------------------------------

function renderFacts() {
  const toolbar = h("div", { class: "toolbar" },
    h("div", { class: "segmented", role: "group", "aria-label": "Which facts" },
      [["current", "Now"], ["past", "Past"], ["all", "All"]].map(([key, label]) => h("button", {
        "aria-pressed": S.factState === key ? "true" : "false",
        onclick: () => showFacts(key),
      }, label))),
    h("input", {
      class: "filter", id: "filter", type: "search", placeholder: "Filter this list", "aria-label": "Filter this list", value: S.filter,
      oninput: (event) => {
        S.filter = event.target.value;
        const results = document.getElementById("fact-results");
        if (results) results.replaceChildren(renderFactGroups());
      },
    }),
    h("span", { class: "toolbar-note" }, `${S.facts.length.toLocaleString()} loaded`));
  return h("div", {}, toolbar, h("div", { id: "fact-results" }, renderFactGroups()));

  function renderFactGroups() {
    const needle = S.filter.toLowerCase();
    const list = S.facts.filter((f) => !needle
      || f.statement.toLowerCase().includes(needle)
      || f.subject_name.toLowerCase().includes(needle)
      || humanize(f.predicate).toLowerCase().includes(needle));
    if (!list.length) {
      if (S.filter) return empty("No match", "Nothing in this list matches. Clear the filter, or ask above to search the whole space.");
      const words = { current: "Nothing is known now", past: "Nothing has been replaced yet", all: "No facts yet" };
      return empty(words[S.factState], "Facts your agent saves here will show up in this list.");
    }
    const groups = new Map();
    for (const fact of list) {
      if (!groups.has(fact.subject_id)) groups.set(fact.subject_id, { name: fact.subject_name, kind: fact.subject_kind, facts: [] });
      groups.get(fact.subject_id).facts.push(fact);
    }
    return h("div", {},
      [...groups.values()].map((group) => h("section", { class: "group" },
        h("div", { class: "group-title" }, group.name, h("span", { class: "kind-tag" }, humanize(group.kind))),
        h("div", { class: "list" }, group.facts.map(factRow)))),
      !S.factsDone && !S.filter && h("button", {
        class: "more",
        onclick: () => guarded(async () => { await loadFacts(false); render(); }),
      }, "Load more"));
  }
}

function factRow(fact) {
  const status = factStatus(fact);
  return h("button", {
    class: `row${status.key === "current" ? "" : " is-past"}`,
    "aria-current": S.drawer?.id === fact.id ? "true" : null,
    onclick: () => openFact(fact.id),
  },
    h("div", { class: "row-text" }, fact.statement),
    h("div", { class: "meta" },
      status.key !== "current" && pill(fact),
      h("span", {}, humanize(fact.predicate)),
      h("span", {}, status.key === "current" ? `since ${date(fact.valid_from)}` : `${date(fact.valid_from)} to ${date(fact.valid_to || fact.invalidated_at) || "now"}`),
      h("span", {}, TRUST[fact.trust][0])));
}

function renderAsk() {
  const { query, results } = S.ask;
  const head = h("div", { class: "toolbar" },
    h("span", {}, "Best matches for ", h("strong", {}, query)),
    h("button", { class: "link-btn", onclick: () => { S.ask = null; render(); } }, "Clear"),
    h("span", { class: "toolbar-note" }, "This is what your agent would get"));
  if (results === null) return h("div", {}, head, loading());
  if (!results.length) return h("div", {}, head, empty("Nothing found", "No fact that is true now matches this question."));
  return h("div", {}, head, h("div", { class: "list" }, results.map(({ fact }, index) => h("button", {
    class: "row", onclick: () => openFact(fact.id),
  },
    h("div", { class: "row-text" }, h("span", { class: `rank${index === 0 ? " top" : ""}` }, index + 1), fact.statement),
    h("div", { class: "meta" }, h("span", {}, humanize(fact.predicate)), h("span", {}, `since ${date(fact.valid_from)}`), h("span", {}, TRUST[fact.trust][0]))))));
}

// ---- Map --------------------------------------------------------------------------------------

function buildGraph(facts) {
  const nodes = new Map();
  const touch = (id, name, kind) => {
    if (!nodes.has(id)) nodes.set(id, { id, name, kind, x: 0, y: 0, vx: 0, vy: 0, degree: 0, facts: [] });
    return nodes.get(id);
  };
  const edges = [];
  for (const fact of facts) {
    const from = touch(fact.subject_id, fact.subject_name, fact.subject_kind);
    from.facts.push(fact);
    if (fact.object_id) {
      const to = touch(fact.object_id, fact.object_name, fact.object_kind);
      to.facts.push(fact);
      from.degree++;
      to.degree++;
      edges.push({ fact, from, to });
    }
  }
  const list = [...nodes.values()];
  layout(list, edges);
  const kinds = [...new Set(list.map((node) => node.kind))];
  return { nodes: list, edges, kinds, total: facts.length, limited: facts.length >= GRAPH_LIMIT, view: null };
}

// A small force layout, run to the end before drawing, so the map appears settled and does not jiggle.
function layout(nodes, edges) {
  const golden = Math.PI * (3 - Math.sqrt(5));
  nodes.forEach((node, index) => {
    const radius = 26 * Math.sqrt(index + 1);
    node.x = radius * Math.cos(index * golden);
    node.y = radius * Math.sin(index * golden);
  });
  const rounds = nodes.length > 200 ? 140 : 280;
  for (let round = 0; round < rounds; round++) {
    const heat = 1 - round / rounds;
    for (let a = 0; a < nodes.length; a++) {
      for (let b = a + 1; b < nodes.length; b++) {
        const dx = nodes[a].x - nodes[b].x;
        const dy = nodes[a].y - nodes[b].y;
        const square = Math.max(dx * dx + dy * dy, 64);
        const push = 16000 / square;
        nodes[a].vx += dx * push / Math.sqrt(square);
        nodes[a].vy += dy * push / Math.sqrt(square);
        nodes[b].vx -= dx * push / Math.sqrt(square);
        nodes[b].vy -= dy * push / Math.sqrt(square);
      }
    }
    for (const { from, to } of edges) {
      const dx = to.x - from.x;
      const dy = to.y - from.y;
      const distance = Math.max(Math.hypot(dx, dy), 1);
      const pull = (distance - 170) * 0.03;
      from.vx += dx / distance * pull;
      from.vy += dy / distance * pull;
      to.vx -= dx / distance * pull;
      to.vy -= dy / distance * pull;
    }
    for (const node of nodes) {
      node.vx -= node.x * 0.012;
      node.vy -= node.y * 0.012;
      node.x += Math.max(-30, Math.min(30, node.vx)) * heat;
      node.y += Math.max(-30, Math.min(30, node.vy)) * heat;
      node.vx *= 0.55;
      node.vy *= 0.55;
    }
  }
}

const nodeRadius = (node) => 7 + Math.min(node.degree, 12) * 1.25;
const nodeColor = (graph, node) => `var(--node-${(graph.kinds.indexOf(node.kind) % 6) + 1})`;
const shorten = (text, size) => (text.length > size ? `${text.slice(0, size - 1)}…` : text);

function renderMap() {
  const graph = S.graph;
  if (!graph) return loading();
  if (!graph.nodes.length) return empty("Nothing to draw yet", "The map shows the people and things in the facts known now.");

  const svg = s("svg", { class: "map", role: "img", "aria-label": `Map of ${graph.nodes.length} people and things` });
  const world = s("g", {});
  svg.append(world);
  const showAllLabels = graph.nodes.length <= 70;

  const edgeEls = graph.edges.map((edge) => {
    const line = s("line", { class: "edge" });
    const hit = s("line", { class: "edge-hit", onclick: () => openFact(edge.fact.id) }, s("title", {}, edge.fact.statement));
    const label = s("text", { class: "edge-label" }, humanize(edge.fact.predicate));
    label.style.display = "none";
    world.append(line, hit, label);
    return { edge, line, hit, label };
  });
  const nodeEls = graph.nodes.map((node) => {
    const text = s("text", { dy: nodeRadius(node) + 15 }, shorten(node.name, 24));
    if (!showAllLabels && node.degree < 2) text.style.display = "none";
    const group = s("g", { class: "node", tabindex: 0, role: "button", "aria-label": node.name },
      s("title", {}, `${node.name}: ${node.facts.length} fact${node.facts.length === 1 ? "" : "s"}`),
      s("circle", { r: nodeRadius(node), css: { fill: nodeColor(graph, node) } }),
      text);
    group.addEventListener("keydown", (event) => { if (event.key === "Enter") selectNode(node); });
    world.append(group);
    return { node, group, text };
  });

  function place() {
    for (const { edge, line, hit, label } of edgeEls) {
      for (const el of [line, hit]) {
        el.setAttribute("x1", edge.from.x);
        el.setAttribute("y1", edge.from.y);
        el.setAttribute("x2", edge.to.x);
        el.setAttribute("y2", edge.to.y);
      }
      label.setAttribute("x", (edge.from.x + edge.to.x) / 2);
      label.setAttribute("y", (edge.from.y + edge.to.y) / 2 - 4);
    }
    for (const { node, group } of nodeEls) group.setAttribute("transform", `translate(${node.x} ${node.y})`);
  }

  function highlight() {
    const chosen = S.selectedNode;
    const near = new Set();
    if (chosen) {
      near.add(chosen);
      for (const { from, to } of graph.edges) {
        if (from.id === chosen) near.add(to.id);
        if (to.id === chosen) near.add(from.id);
      }
    }
    for (const { node, group, text } of nodeEls) {
      group.classList.toggle("dim", Boolean(chosen) && !near.has(node.id));
      group.classList.toggle("on", node.id === chosen);
      if (!showAllLabels) text.style.display = node.degree >= 2 || near.has(node.id) ? "" : "none";
    }
    for (const { edge, line, label } of edgeEls) {
      const on = chosen && (edge.from.id === chosen || edge.to.id === chosen);
      line.classList.toggle("on", Boolean(on));
      line.classList.toggle("dim", Boolean(chosen) && !on);
      label.style.display = on ? "" : "none";
    }
  }

  function selectNode(node) {
    S.selectedNode = node.id;
    S.drawer = { type: "entity", node };
    highlight();
    renderDrawer();
  }

  const view = graph.view || { x: 0, y: 0, k: 1 };
  const applyView = () => {
    world.setAttribute("transform", `translate(${view.x} ${view.y}) scale(${view.k})`);
    graph.view = view;
  };
  function fit() {
    const box = svg.getBoundingClientRect();
    if (!box.width) return;
    const xs = graph.nodes.map((node) => node.x);
    const ys = graph.nodes.map((node) => node.y);
    const [minX, maxX, minY, maxY] = [Math.min(...xs) - 90, Math.max(...xs) + 90, Math.min(...ys) - 50, Math.max(...ys) + 60];
    view.k = Math.min(1.05, box.width / (maxX - minX), box.height / (maxY - minY));
    view.x = box.width / 2 - ((minX + maxX) / 2) * view.k;
    view.y = box.height / 2 - ((minY + maxY) / 2) * view.k;
    applyView();
  }
  function zoom(factor, cx, cy) {
    const box = svg.getBoundingClientRect();
    const x = cx ?? box.width / 2;
    const y = cy ?? box.height / 2;
    const next = Math.max(0.2, Math.min(4, view.k * factor));
    view.x = x - ((x - view.x) / view.k) * next;
    view.y = y - ((y - view.y) / view.k) * next;
    view.k = next;
    applyView();
  }

  svg.addEventListener("wheel", (event) => {
    event.preventDefault();
    const box = svg.getBoundingClientRect();
    zoom(Math.exp(-event.deltaY * 0.0016), event.clientX - box.left, event.clientY - box.top);
  }, { passive: false });

  let drag = null;
  svg.addEventListener("pointerdown", (event) => {
    const hitNode = nodeEls.find(({ group }) => group.contains(event.target));
    if (event.target.classList?.contains("edge-hit")) return;
    drag = { node: hitNode?.node || null, sx: event.clientX, sy: event.clientY, moved: false, ox: hitNode?.node.x, oy: hitNode?.node.y, vx: view.x, vy: view.y };
    svg.setPointerCapture(event.pointerId);
    if (!hitNode) svg.classList.add("panning");
  });
  svg.addEventListener("pointermove", (event) => {
    if (!drag) return;
    const dx = event.clientX - drag.sx;
    const dy = event.clientY - drag.sy;
    if (Math.hypot(dx, dy) > 3) drag.moved = true;
    if (!drag.moved) return;
    if (drag.node) {
      drag.node.x = drag.ox + dx / view.k;
      drag.node.y = drag.oy + dy / view.k;
      place();
    } else {
      view.x = drag.vx + dx;
      view.y = drag.vy + dy;
      applyView();
    }
  });
  svg.addEventListener("pointerup", () => {
    svg.classList.remove("panning");
    if (drag && !drag.moved) {
      if (drag.node) selectNode(drag.node);
      else if (S.selectedNode) closeSelection();
    }
    drag = null;
  });
  function closeSelection() {
    S.selectedNode = null;
    S.drawer = null;
    highlight();
    renderDrawer();
  }

  place();
  highlight();
  const wrap = h("div", {},
    svg,
    h("div", { class: "map-legend" }, graph.kinds.slice(0, 6).map((kind, index) => h("span", { class: "legend-item" },
      h("span", { class: "legend-swatch", css: { background: `var(--node-${index + 1})` } }), humanize(kind)))),
    h("div", { class: "map-note" }, graph.limited ? `Newest ${GRAPH_LIMIT} facts. Click anything for detail.` : "Click anything for detail. Drag to move, scroll to zoom."),
    h("div", { class: "map-controls" },
      h("button", { "aria-label": "Zoom in", title: "Zoom in", onclick: () => zoom(1.25) }, "+"),
      h("button", { "aria-label": "Zoom out", title: "Zoom out", onclick: () => zoom(0.8) }, "−"),
      h("button", { "aria-label": "Fit to window", title: "Fit to window", onclick: fit }, "⤢")));
  if (graph.view) applyView();
  else requestAnimationFrame(fit);
  return wrap;
}

// ---- Timeline ---------------------------------------------------------------------------------

function renderTimeline() {
  const facts = S.timeline;
  if (!facts) return loading();
  if (!facts.length) return empty("No history yet", "When a fact changes, you will see the old and new versions here.");

  const now = Date.now();
  const lanes = new Map();
  for (const fact of facts) {
    const key = `${fact.subject_id}|${fact.predicate}`;
    if (!lanes.has(key)) lanes.set(key, { subject: fact.subject_name, predicate: fact.predicate, facts: [] });
    lanes.get(key).facts.push(fact);
  }
  const start = Math.min(...facts.map((f) => new Date(f.valid_from).getTime()));
  const span = Math.max(now - start, 86400000);
  const min = start - span * 0.02;
  const width = now - min;
  const at = (time) => ((Math.min(time, now) - min) / width) * 100;
  const order = [...lanes.values()].sort((a, b) => b.facts.length - a.facts.length
    || Math.max(...b.facts.map((f) => new Date(f.valid_from).getTime())) - Math.max(...a.facts.map((f) => new Date(f.valid_from).getTime())));

  const ticks = [0, 0.25, 0.5, 0.75, 1].map((part) => {
    const time = min + width * part;
    return h("span", { css: { left: `${part * 100}%` } }, part === 1 ? "Today" : date(new Date(time).toISOString()));
  });

  return h("div", {},
    h("div", { class: "toolbar" },
      h("div", { class: "timeline-key" },
        h("span", {}, h("span", { class: "key-swatch", css: { background: "var(--accent)" } }), "True now"),
        h("span", {}, h("span", { class: "key-swatch", css: { background: "var(--past-soft)", "box-shadow": "inset 0 0 0 1px var(--line-strong)" } }), "Replaced or ended"),
        h("span", {}, h("span", { class: "key-swatch", css: { background: "var(--red-soft)" } }), "Retracted")),
      h("span", { class: "toolbar-note" }, facts.length >= GRAPH_LIMIT ? `Newest ${GRAPH_LIMIT} facts. What changed comes first.` : "What changed comes first")),
    h("div", { class: "axis" }, ticks),
    order.map((lane) => h("div", { class: "lane" },
      h("div", { class: "lane-label", title: `${lane.subject}: ${humanize(lane.predicate)}` },
        h("span", { class: "lane-subject" }, lane.subject),
        h("span", { class: "lane-predicate" }, humanize(lane.predicate))),
      h("div", { class: "lane-track" }, lane.facts.map((fact) => {
        const status = factStatus(fact);
        const left = at(new Date(fact.valid_from).getTime());
        const end = status.key === "retracted" ? new Date(fact.invalidated_at).getTime() : fact.valid_to ? new Date(fact.valid_to).getTime() : now;
        const kind = status.key === "current" ? "current" : status.key === "retracted" ? "retracted" : "past";
        return h("button", {
          class: `bar ${kind}`, title: `${fact.statement} (${date(fact.valid_from)} to ${fact.valid_to ? date(fact.valid_to) : "now"})`,
          "aria-current": S.drawer?.id === fact.id ? "true" : null,
          css: { left: `${left}%`, width: `calc(${Math.max(at(end) - left, 0)}% - 2px)` },
          onclick: () => openFact(fact.id),
        }, fact.statement);
      })))));
}

// ---- Review -----------------------------------------------------------------------------------

function renderReview() {
  if (S.review === null || S.pageReview === null) return loading();
  if (S.reviewDenied) return empty("Review needs the admin key", "This page is using a key that cannot review. Run memoryos setup to create the admin key.");
  if (!S.review.length && !S.pageReview.length) {
    return empty("All clear", "Nothing is waiting. Facts and page edits held back from your agent's memory will show up here for you to decide.");
  }
  const both = S.review.length > 0 && S.pageReview.length > 0;
  return h("div", {},
    h("div", { class: "toolbar" }, h("span", { class: "toolbar-note", css: { "margin-left": "0" } },
      "Your agent cannot recall these until you approve them. Check the source: outside content can try to plant false facts.")),
    S.pageReview.length > 0 && h("section", { class: "group" },
      both && h("div", { class: "group-title" }, "Page edits", h("span", { class: "kind-tag" }, S.pageReview.length)),
      S.pageReview.map(renderPageEdit)),
    S.review.length > 0 && h("section", { class: "group" },
      both && h("div", { class: "group-title" }, "Facts", h("span", { class: "kind-tag" }, S.review.length)),
      renderFactReview()));
}

function renderFactReview() {
  return S.review.map(({ fact, replaces, source }) => h("article", { class: "card" },
      h("div", { class: "card-title" }, fact.statement),
      h("div", { class: "meta" }, h("span", {}, humanize(fact.predicate)), h("span", {}, TRUST[fact.trust][0]), h("span", {}, `added ${ago(fact.recorded_at)}`)),
      replaces && h("div", { class: "quote" }, h("span", { class: "quote-label" }, "Would replace"), replaces.statement),
      source && h("div", { class: "quote" }, h("span", { class: "quote-label" }, `Source: ${source.source}`), source.content),
      h("div", { class: "actions" },
        h("button", { class: "btn primary", onclick: (event) => { event.target.disabled = true; decide(fact.id, "approve"); } }, "Approve"),
        h("button", { class: "btn danger", onclick: (event) => { event.target.disabled = true; decide(fact.id, "reject"); } }, "Reject"),
        h("button", { class: "link-btn", css: { "margin-left": "auto" }, onclick: () => openFact(fact.id) }, "Details"))));
}

function renderPageEdit({ version, current }) {
  const stale = version.base_version !== current;
  const conflict = S.pageConflicts.has(pendingKey(version)) || stale;
  const isNew = version.base_version === null;
  const pruned = !isNew && version.base_body === null;
  let base;
  if (isNew) base = current === null ? "New page" : `New page, but version ${current} now exists`;
  else base = `Edits version ${version.base_version}${stale ? `, current is ${current ?? "none"}` : ""}`;
  return h("article", { class: "card" },
    h("div", { class: "card-title" }, version.title),
    h("div", { class: "meta" },
      h("span", { class: "slug" }, version.slug),
      h("span", {}, `version ${version.version}`),
      h("span", {}, base),
      h("span", {}, TRUST[version.trust]?.[0] || version.trust),
      h("span", {}, `by ${version.edited_by}`),
      h("span", {}, `added ${ago(version.edited_at)}`)),
    version.summary && h("div", { class: "quote" }, h("span", { class: "quote-label" }, "What changed"), version.summary),
    conflict && h("div", { class: "alert", role: "alert" }, VERSION_CONFLICT),
    pruned && h("p", { class: "help diff-note" }, `Version ${version.base_version} is no longer kept, so the whole new text is shown.`),
    renderDiff(isNew || pruned ? "" : version.base_body, version.body),
    h("div", { class: "actions" },
      h("button", { class: "btn primary", onclick: (event) => { event.target.disabled = true; decidePage(version, "approve"); } }, "Approve"),
      h("button", { class: "btn danger", onclick: (event) => { event.target.disabled = true; decidePage(version, "reject"); } }, "Reject"),
      current !== null && h("button", {
        class: "link-btn", css: { "margin-left": "auto" },
        onclick: () => openPage(version.slug),
      }, "Read current page")));
}

// ---- Line diff --------------------------------------------------------------------------------

// A final line end does not start another line.
const splitLines = (text) => (text === "" ? [] : text.replace(/\r\n/g, "\n").replace(/\n$/, "").split("\n"));

// A longest common subsequence over lines, after trimming what both ends share. A change too large
// for the table is shown as every old line removed and every new line added.
function lineDiff(before, after) {
  const a = splitLines(before);
  const b = splitLines(after);
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) { endA--; endB--; }
  const head = a.slice(0, start).map((text) => ({ kind: "same", text }));
  const tail = a.slice(endA).map((text) => ({ kind: "same", text }));
  const x = a.slice(start, endA);
  const y = b.slice(start, endB);
  const n = x.length;
  const m = y.length;
  const middle = [];
  if ((n + 1) * (m + 1) > DIFF_MAX_CELLS) {
    for (const text of x) middle.push({ kind: "del", text });
    for (const text of y) middle.push({ kind: "add", text });
    return head.concat(middle, tail);
  }
  const width = m + 1;
  const table = new Uint32Array((n + 1) * width);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      table[i * width + j] = x[i] === y[j]
        ? table[(i + 1) * width + j + 1] + 1
        : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
  }
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (x[i] === y[j]) { middle.push({ kind: "same", text: x[i] }); i++; j++; }
    else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) middle.push({ kind: "del", text: x[i++] });
    else middle.push({ kind: "add", text: y[j++] });
  }
  while (i < n) middle.push({ kind: "del", text: x[i++] });
  while (j < m) middle.push({ kind: "add", text: y[j++] });
  return head.concat(middle, tail);
}

function renderDiff(before, after) {
  const lines = lineDiff(before, after);
  const added = lines.filter((line) => line.kind === "add").length;
  const removed = lines.filter((line) => line.kind === "del").length;
  const rows = [];
  let at = 0;
  while (at < lines.length) {
    if (lines[at].kind !== "same") { rows.push(diffLine(lines[at])); at++; continue; }
    let end = at;
    while (end < lines.length && lines[end].kind === "same") end++;
    const keepBefore = at === 0 ? 0 : DIFF_CONTEXT;
    const keepAfter = end === lines.length ? 0 : DIFF_CONTEXT;
    if (end - at > keepBefore + keepAfter + 1) {
      for (let k = at; k < at + keepBefore; k++) rows.push(diffLine(lines[k]));
      const hidden = end - at - keepBefore - keepAfter;
      rows.push(h("div", { class: "diff-gap" }, `${hidden} unchanged line${hidden === 1 ? "" : "s"}`));
      for (let k = end - keepAfter; k < end; k++) rows.push(diffLine(lines[k]));
    } else {
      for (let k = at; k < end; k++) rows.push(diffLine(lines[k]));
    }
    at = end;
  }
  return h("div", { class: "diff-wrap" },
    h("div", { class: "diff-head" },
      h("span", { class: "diff-count add" }, `+${added}`),
      h("span", { class: "diff-count del" }, `-${removed}`),
      h("span", {}, added || removed ? "lines changed" : "The text is unchanged")),
    rows.length > 0 && h("div", { class: "diff", role: "table", "aria-label": "Changes to the page text" }, rows));
}

function diffLine({ kind, text }) {
  const sign = { add: "+", del: "-", same: " " }[kind];
  const label = { add: "Added", del: "Removed", same: "Unchanged" }[kind];
  return h("div", { class: `diff-line ${kind}`, role: "row", "aria-label": label },
    h("span", { class: "diff-sign", "aria-hidden": "true" }, sign),
    h("span", { class: "diff-text" }, text));
}

// ---- Pages ------------------------------------------------------------------------------------

const VERSION_STATUS = {
  active: ["Current", "current"],
  superseded: ["Older", "past"],
  quarantined: ["Waiting for review", "waiting"],
  rejected: ["Rejected", "bad"],
};

// "New page": a slug, then the editor opens on an empty page; saving creates it.
function renderNewPage() {
  if (S.newPage === null) {
    return h("button", { class: "btn", onclick: () => { S.newPage = ""; render(); document.getElementById("new-slug")?.focus(); } }, "New page");
  }
  const start = () => {
    const slug = S.newPage.trim();
    if (!/^[a-z0-9][a-z0-9._-]{0,127}$/.test(slug)) {
      toast("Use lowercase letters, digits, '.', '_' and '-', such as deploy-guide", true);
      return;
    }
    S.newPage = null;
    editInEditor(slug);
  };
  return h("span", { class: "new-page" },
    h("input", {
      id: "new-slug", class: "filter", placeholder: "new-page-name", "aria-label": "New page name", value: S.newPage,
      oninput: (event) => { S.newPage = event.target.value; },
      onkeydown: (event) => { if (event.key === "Enter") start(); if (event.key === "Escape") { S.newPage = null; render(); } },
    }),
    h("button", { class: "btn primary", onclick: start }, "Open in editor"),
    h("button", { class: "link-btn", onclick: () => { S.newPage = null; render(); } }, "Cancel"));
}

function renderPages() {
  if (S.page) return renderPage();
  if (S.pages === null) return loading();
  const pending = pendingPagesIn(S.scope);
  const waiting = pending > 0 && h("button", { class: "link-btn", onclick: () => selectTab("review") },
    `${pending} edit${pending === 1 ? "" : "s"} waiting for review`);
  if (!S.pages.length) {
    return h("div", { class: "empty" }, h("h2", {}, "No pages yet"),
      h("p", {}, "Runbooks, guides and other pages for this space show up here. Start one with New page, or let your agent write one."),
      h("p", { css: { "margin-top": "10px" } }, renderNewPage()),
      waiting && h("p", { css: { "margin-top": "10px" } }, waiting));
  }
  const toolbar = h("div", { class: "toolbar" },
    h("input", {
      class: "filter", type: "search", placeholder: "Filter pages", "aria-label": "Filter pages", value: S.pageFilter,
      oninput: (event) => {
        S.pageFilter = event.target.value;
        document.getElementById("page-results")?.replaceChildren(renderPageRows());
      },
    }),
    waiting,
    renderNewPage(),
    h("span", { class: "toolbar-note" }, S.pages.length >= PAGE_LIST_LIMIT ? `First ${PAGE_LIST_LIMIT} pages by name` : `${S.pages.length} page${S.pages.length === 1 ? "" : "s"}`));
  return h("div", {}, toolbar, h("div", { id: "page-results" }, renderPageRows()));

  function renderPageRows() {
    const needle = S.pageFilter.toLowerCase();
    const list = S.pages.filter((page) => !needle || page.title.toLowerCase().includes(needle) || page.slug.includes(needle));
    if (!list.length) return empty("No match", "No page title or name matches this filter.");
    return h("div", { class: "list" }, list.map((page) => h("button", { class: "row", onclick: () => openPage(page.slug) },
      h("div", { class: "row-text" }, page.title),
      h("div", { class: "meta" },
        h("span", { class: "slug" }, page.slug),
        h("span", {}, `version ${page.version}`),
        h("span", {}, `edited ${ago(page.edited_at)}`)))));
  }
}

function renderPage() {
  const { slug, version, data, versions } = S.page;
  const back = h("div", { class: "toolbar" },
    h("button", { class: "link-btn", onclick: closePage }, "All pages"),
    h("span", { class: "toolbar-note slug" }, slug));
  if (!data || !versions) return h("div", {}, back, loading());
  const latest = versions.find((item) => item.status === "active");
  const older = version !== null && data.status !== "active";
  return h("div", {}, back,
    h("div", { class: "page-layout" },
      h("article", { class: "card page-card" },
        older && h("div", { class: "notice" },
          h("span", {}, `You are reading version ${data.version}, which was replaced.`),
          latest && h("button", { class: "link-btn", onclick: () => openPage(slug) }, `Read version ${latest.version}, the current one`)),
        h("div", { class: "page-head" },
          h("h2", { class: "page-title" }, data.title),
          !older && h("button", {
            class: "btn",
            disabled: S.editing === slug,
            onclick: () => editInEditor(slug),
          }, S.editing === slug ? "Open in your editor" : "Open in editor")),
        h("div", { class: "meta" },
          h("span", { class: `pill ${older ? "past" : "current"}` }, older ? "Older version" : "Current"),
          h("span", {}, `version ${data.version}`),
          h("span", {}, TRUST[data.trust]?.[0] || data.trust),
          h("span", {}, `by ${data.edited_by}`),
          h("span", { title: data.edited_at }, `edited ${ago(data.edited_at)}`)),
        data.summary && h("div", { class: "quote" }, h("span", { class: "quote-label" }, "What changed"), data.summary),
        renderPageBody(data.body)),
      h("aside", { class: "card versions" },
        h("h3", {}, "Versions"),
        versions.map((item) => renderVersionRow(item, data.version)))));
}

function isDark() {
  const theme = document.documentElement.dataset.theme;
  return theme === "dark" || (!theme && matchMedia("(prefers-color-scheme: dark)").matches);
}

// Rendered markdown with diagrams, or the raw text. The choice is remembered.
function renderPageBody(body) {
  const view = S.pageView;
  const pick = (value, label) => h("button", {
    class: `seg${view === value ? " on" : ""}`, "aria-pressed": view === value ? "true" : "false",
    onclick: () => { S.pageView = value; storageSet(localStorage, "memoryos-page-view", value); render(); },
  }, label);
  const switcher = h("div", { class: "segmented", role: "group", "aria-label": "Page view" },
    pick("rendered", "Rendered"), pick("markdown", "Markdown"));
  if (view === "markdown") {
    return h("div", {}, switcher, h("pre", { class: "page-body", "aria-label": "Page text" }, body));
  }
  const rendered = Markdown.render(body);
  rendered.classList.add("page-rendered");
  Markdown.renderMermaid(rendered, isDark());
  return h("div", {}, switcher, rendered);
}

function renderVersionRow(item, shown) {
  const [label, kind] = VERSION_STATUS[item.status] || [item.status, "past"];
  const readable = item.status === "active" || item.status === "superseded";
  const content = [
    h("div", { class: "version-top" }, h("strong", {}, `Version ${item.version}`), h("span", { class: `pill ${kind}` }, label)),
    h("div", { class: "meta" }, h("span", {}, `by ${item.edited_by}`), h("span", {}, ago(item.edited_at))),
    item.summary && h("div", { class: "version-summary" }, item.summary),
  ];
  if (item.status === "quarantined") {
    return h("button", { class: "version-row", title: "Open Review to decide", onclick: () => selectTab("review") }, content);
  }
  if (!readable) return h("div", { class: "version-row muted" }, content);
  return h("button", {
    class: "version-row", "aria-current": item.version === shown ? "true" : null,
    onclick: () => openPage(S.page.slug, item.status === "active" ? null : item.version),
  }, content);
}

function renderNotes() {
  if (S.notes === null) return loading();
  if (!S.notes.length) return empty("No notes", "This agent has not written any working notes.");
  return h("div", {}, S.notes.map((block) => h("article", { class: "card" },
    h("div", { class: "card-title" }, humanize(block.label)),
    h("div", { class: "meta" }, h("span", {}, `version ${block.version}`), h("span", {}, `updated ${ago(block.updated_at)}`),
      h("span", {}, `${block.content.length.toLocaleString()} of ${block.max_chars.toLocaleString()} characters`)),
    h("div", { class: "meter" }, h("span", { css: { width: `${Math.min(100, (block.content.length / block.max_chars) * 100)}%` } })),
    h("div", { class: "note-text" }, block.content))));
}

// ---- Drawer -----------------------------------------------------------------------------------

function renderDrawer() {
  document.getElementById("drawer")?.remove();
  if (!S.drawer || !S.token) return;
  const content = S.drawer.type === "entity" ? renderEntityDrawer(S.drawer.node) : renderFactDrawer(S.drawer);
  document.body.append(h("aside", { class: "drawer", id: "drawer", "aria-label": "Detail" },
    h("div", { class: "drawer-head" },
      h("strong", {}, S.drawer.type === "entity" ? humanize(S.drawer.node.kind) : "Fact"),
      h("span", { class: "spacer" }),
      h("button", { class: "icon-btn", "aria-label": "Close", title: "Close (Esc)", onclick: closeDrawer }, icon(ICONS.close))),
    h("div", { class: "drawer-body" }, content)));
}

function renderEntityDrawer(node) {
  return h("div", {},
    h("div", { class: "drawer-title" }, node.name),
    h("div", { class: "meta" }, `${node.facts.length} fact${node.facts.length === 1 ? "" : "s"} known now`),
    h("div", { class: "section" }, h("h3", {}, "What is known"),
      node.facts.map((fact) => h("button", { class: "mini-row", onclick: () => openFact(fact.id) }, fact.statement))));
}

function renderFactDrawer({ fact, history, sources }) {
  if (!fact) return loading();
  const trust = TRUST[fact.trust];
  const status = factStatus(fact);
  return h("div", {},
    h("div", { class: "drawer-title" }, fact.statement),
    h("div", { class: "meta" }, pill(fact)),
    h("dl", { class: "fields" },
      h("dt", {}, "About"), h("dd", {}, humanize(fact.predicate)),
      h("dt", {}, "True from"), h("dd", {}, date(fact.valid_from)),
      h("dt", {}, "True until"), h("dd", {}, fact.valid_to ? date(fact.valid_to) : status.key === "current" ? "Still true" : "Not set"),
      fact.expires_at && [h("dt", {}, "Expires"), h("dd", {}, date(fact.expires_at))],
      h("dt", {}, "Learned"), h("dd", {}, `${date(fact.recorded_at)} (${ago(fact.recorded_at)})`),
      fact.invalidated_at && [h("dt", {}, "Retracted"), h("dd", {}, date(fact.invalidated_at))],
      h("dt", {}, "Trust"), h("dd", {}, trust[0], h("div", { class: "help" }, trust[1])),
      h("dt", {}, "Space"), h("dd", {}, fact.scope)),
    history && history.length > 1 && h("div", { class: "section" }, h("h3", {}, "How this changed"),
      h("div", { class: "chain" }, history.map((item) => h("button", {
        class: `chain-item${factStatus(item).key === "current" ? " now" : ""}${item.id === fact.id ? " here" : ""}`,
        onclick: () => openFact(item.id),
      },
        h("div", { class: "chain-text" }, item.statement),
        h("div", { class: "meta" }, `${date(item.valid_from)} to ${item.valid_to ? date(item.valid_to) : "now"}`))))),
    sources && sources.length > 0 && h("div", { class: "section" }, h("h3", {}, "Where it came from"),
      sources.map((episode) => h("div", { class: "quote" },
        h("span", { class: "quote-label" }, `${episode.source}, ${date(episode.occurred_at)}`), episode.content))),
    fact.status === "quarantined" && h("div", { class: "actions" },
      h("button", { class: "btn primary", onclick: () => decide(fact.id, "approve") }, "Approve"),
      h("button", { class: "btn danger", onclick: () => decide(fact.id, "reject") }, "Reject")));
}

// ---- Start ------------------------------------------------------------------------------------

function toggleTheme() {
  const dark = document.documentElement.dataset.theme === "dark"
    || (!document.documentElement.dataset.theme && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.dataset.theme = dark ? "light" : "dark";
  storageSet(localStorage, "memoryos-theme", document.documentElement.dataset.theme);
}

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && S.drawer) { closeDrawer(); return; }
  const typing = /^(INPUT|SELECT|TEXTAREA)$/.test(event.target.tagName);
  if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
  if (event.key === "/") { event.preventDefault(); document.getElementById("ask")?.focus(); }
  const tabs = { 1: "facts", 2: "map", 3: "timeline", 4: "review" };
  if (tabs[event.key] && S.scope) selectTab(tabs[event.key]);
});

// Pasting the new address after `memoryos ui` restarts changes only the fragment, so nothing reloads.
addEventListener("hashchange", () => { if (/t=/.test(location.hash)) location.reload(); });

async function start() {
  S.pageView = storageGet(localStorage, "memoryos-page-view") === "markdown" ? "markdown" : "rendered";
  const theme = storageGet(localStorage, "memoryos-theme");
  if (theme) document.documentElement.dataset.theme = theme;
  const match = location.hash.match(/t=([^&]+)/);
  if (match) {
    S.token = decodeURIComponent(match[1]);
    storageSet(sessionStorage, "memoryos-token", S.token);
    history.replaceState(null, "", location.pathname);
  } else {
    S.token = storageGet(sessionStorage, "memoryos-token") || "";
  }
  render();
  if (!S.token) return;
  await guarded(async () => {
    await Promise.all([loadScopes(), loadHealth()]);
    S.signature = JSON.stringify([S.scopes, [...S.pendingPages]]);
    render();
    await loadTab();
    render();
  });
  setInterval(() => { if (!document.hidden) refresh(true); }, REFRESH_MS);
  addEventListener("focus", () => refresh(true));
}

start();
