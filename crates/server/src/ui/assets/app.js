"use strict";

// Fact and page text is untrusted. Everything reaches the page through h(), which only ever sets text
// nodes and attributes, so a planted statement cannot become markup. Page bodies are markdown, and they
// are shown as plain text, never rendered.

const SVG_NS = "http://www.w3.org/2000/svg";
const GRAPH_LIMIT = 300;
const PAGE = 100;
const REFRESH_MS = 30000;
const PAGE_LIST_LIMIT = 500;
const INSIGHT_LIMIT = 500;
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
  pendingInsights: new Map(),
  insights: null,
  insightState: "active",
  insightsMissing: false,
  insightReview: null,
  editing: null,
  pageView: "rendered",
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

// Page versions and insights waiting for review, which the server counts apart from facts.
const pendingPagesIn = (scope) => S.pendingPages.get(scope) || 0;
const pendingInsightsIn = (scope) => S.pendingInsights.get(scope) || 0;
const waitingIn = (summary) => summary.quarantined + pendingPagesIn(summary.scope) + pendingInsightsIn(summary.scope);
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
  const [body, pending, pendingInsights] = await Promise.all([
    api("scopes").catch((error) => {
      if (error.status !== 404) throw error;
      throw Object.assign(new Error("Your server is older than this page. Update it with: docker compose -f ~/.config/memoryos/docker-compose.yml up -d --build"), { status: 0 });
    }),
    loadPendingPages(),
    loadPendingInsights(),
  ]);
  S.scopes = body.scopes;
  S.truncated = body.truncated;
  S.pendingPages = pending;
  S.pendingInsights = pendingInsights;
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

// Quarantined insights per scope. Empty for a key that cannot review and for a server from before
// insights.
async function loadPendingInsights() {
  const counts = new Map();
  try {
    const body = await api(`insights/quarantine?${new URLSearchParams({ scopes: "*", limit: 200 })}`);
    for (const insight of body.insights) counts.set(insight.scope, (counts.get(insight.scope) || 0) + 1);
  } catch {
    // As for pages: the review list reports real failures.
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
  if (S.tab === "insights") await loadInsights();
  if (S.tab === "notes") {
    S.notes = (await api(`agents/${encodeURIComponent(scopeParts(S.scope).name)}/blocks`)).blocks;
  }
}

async function loadReview() {
  S.reviewDenied = false;
  const [facts, pages, insights] = await Promise.all([reviewFacts(), reviewPages(), reviewInsights()]);
  S.review = facts;
  S.pageReview = pages;
  S.insightReview = insights;
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

// Each quarantined insight with its sources as they are now, and the insight it would replace.
async function reviewInsights() {
  let body;
  try {
    body = await api(`insights/quarantine?${new URLSearchParams({ scopes: S.scope, limit: 200 })}`);
  } catch (error) {
    // 403: a key that cannot review, reported with the facts. 404: a server from before insights.
    if (error.status === 403 || error.status === 404) return [];
    throw error;
  }
  return Promise.all(body.insights.map(async (insight) => {
    const [detail, replaces] = await Promise.all([
      api(`insights/${insight.id}`).catch(() => null),
      insight.pending_supersede_of ? api(`insights/${insight.pending_supersede_of}`).catch(() => null) : null,
    ]);
    return { insight, sources: detail?.source_views ?? null, replaces };
  }));
}

async function loadInsights() {
  const params = { scopes: S.scope, limit: INSIGHT_LIMIT };
  if (S.insightState !== "all") params.status = S.insightState;
  const state = S.insightState;
  let insights;
  try {
    insights = (await api(`insights?${new URLSearchParams(params)}`)).insights;
    S.insightsMissing = false;
  } catch (error) {
    if (error.status !== 404) throw error;
    insights = [];
    S.insightsMissing = true;
  }
  // A filter pressed while this loaded wins.
  if (S.insightState === state) S.insights = insights;
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
  S.insights = S.insightReview = null;
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

async function openInsight(id) {
  S.drawer = { type: "insight", id, insight: null };
  renderDrawer();
  await guarded(async () => {
    const insight = await api(`insights/${id}`);
    if (S.drawer?.id !== id) return;
    S.drawer.insight = insight;
    renderDrawer();
  });
}

// After a review or a retire: the counts, the open list and the open insight as they are now.
async function reloadInsights(id) {
  const work = [loadScopes()];
  if (S.tab === "review") work.push(loadReview());
  if (S.tab === "insights") work.push(loadInsights());
  await Promise.all(work);
  if (S.drawer?.type === "insight" && S.drawer.id === id) {
    const insight = await api(`insights/${id}`).catch(() => null);
    if (S.drawer?.id === id) S.drawer.insight = insight;
    if (!insight) S.drawer = null;
  }
}

async function decideInsight(id, action) {
  await guarded(async () => {
    const insight = await api(`insights/quarantine/${id}/${action}`, { method: "POST" });
    if (action === "reject") toast("Rejected. Your agent will not be shown this.");
    else if (insight?.status === "stale") toast("Approved, but a source changed since, so it is stale until your agent looks again.");
    else toast("Approved. Your agent can now recall this.");
    await reloadInsights(id);
  });
  // Also after a failure, so the buttons are enabled again.
  render();
}

function retireInsight(insight) {
  openDialog({
    title: "Retire this insight",
    intro: "Your agent stops seeing it, and insights built on it become stale until your agent looks at them again. This cannot be undone.",
    fields: [
      { name: "reason", label: "Why it is no longer true", placeholder: "We moved off this setup", hint: "Up to 500 characters. Kept in the audit log.",
        check: /^[\s\S]{1,500}$/, problem: "Say why in 1 to 500 characters." },
    ],
    submit: "Retire",
    onSubmit: (reason) => guarded(async () => {
      await api(`insights/${insight.id}/retire`, { method: "POST", body: { reason } });
      toast("Retired. Your agent will not be shown this again.");
      await reloadInsights(insight.id);
      render();
    }),
  });
}

// What a quiet refresh compares to decide whether the open tab needs loading again.
const countsSignature = () => JSON.stringify([S.scopes, [...S.pendingPages], [...S.pendingInsights]]);

async function refresh(quiet) {
  await guarded(async () => {
    await Promise.all([loadScopes(), loadHealth()]);
    const recovered = Boolean(S.offline);
    S.offline = null;
    const signature = countsSignature();
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

const PAGE_NAME = /^[a-z0-9][a-z0-9._-]{0,127}$/;
const SPACE_NAME = /^[A-Za-z0-9_.-]{1,128}$/;

// A modal form on the native <dialog>: a backdrop, focus kept inside, Esc closes it. Each field has
// a label, a hint and a check; `onSubmit` gets the trimmed values once every check passes.
function openDialog({ title, intro, fields, submit, onSubmit }) {
  const inputs = fields.map((field) => h("input", {
    class: "dialog-input", id: `dialog-${field.name}`, placeholder: field.placeholder, autocomplete: "off", spellcheck: "false",
  }));
  const error = h("p", { class: "dialog-error", role: "alert" });
  const close = () => dialog.close();
  const send = () => {
    const values = inputs.map((input) => input.value.trim());
    for (const [i, field] of fields.entries()) {
      if (!field.check.test(values[i])) {
        error.textContent = field.problem;
        inputs[i].focus();
        return;
      }
    }
    close();
    onSubmit(...values);
  };
  const dialog = h("dialog", { class: "dialog", "aria-labelledby": "dialog-title" },
    h("h2", { id: "dialog-title", class: "dialog-title" }, title),
    intro && h("p", { class: "dialog-intro" }, intro),
    fields.map((field, i) => h("label", { class: "dialog-field", for: `dialog-${field.name}` },
      h("span", { class: "dialog-label" }, field.label), inputs[i],
      h("span", { class: "dialog-hint" }, field.hint))),
    error,
    h("div", { class: "dialog-actions" },
      h("button", { class: "btn", onclick: close }, "Cancel"),
      h("button", { class: "btn primary", onclick: send }, submit)));
  dialog.addEventListener("keydown", (event) => { if (event.key === "Enter") { event.preventDefault(); send(); } });
  dialog.addEventListener("close", () => dialog.remove());
  document.body.append(dialog);
  dialog.showModal();
  inputs[0].focus();
}

// "+ New space": a shared space and its first page. The space exists once that page is saved.
function renderNewSpace() {
  const start = () => openDialog({
    title: "New space",
    intro: "A shared space holds knowledge pages, like a team space. It is created when you save its first page.",
    fields: [
      { name: "space", label: "Space name", placeholder: "ops", hint: "Letters, digits, '.', '_' and '-'.", check: SPACE_NAME,
        problem: "Name the space with letters, digits, '.', '_' and '-', such as ops." },
      { name: "page", label: "First page", placeholder: "overview", hint: "Lowercase letters, digits, '.', '_' and '-'.", check: PAGE_NAME,
        problem: "Name the first page with lowercase letters, digits, '.', '_' and '-', such as overview." },
    ],
    submit: "Write first page",
    onSubmit: (space, page) => editInEditor(page, `shared:${space}`),
  });
  return h("button", { class: "space new-space", onclick: start },
    h("span", { class: "space-avatar" }, "+"), h("span", { class: "space-name" }, "New space"));
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
  const tabs = [["facts", "Facts"], ["map", "Map"], ["timeline", "Timeline"], ["pages", "Knowledge"], ["insights", "Insights"], ["review", "Review"]];
  if (summary.blocks > 0) tabs.push(["notes", "Agent notes"]);
  // A server from before insights does not count them.
  const insights = typeof summary.insights === "number" ? summary.insights : null;
  return h("header", { class: "head" },
    h("select", {
      class: "mobile-select", "aria-label": "Memory space",
      onchange: (event) => selectScope(event.target.value),
    }, S.scopes.map((item) => h("option", { value: item.scope, selected: item.scope === S.scope }, item.scope))),
    h("div", { class: "head-top" }, h("h1", {}, name), h("span", { class: "scope-code" }, `${kind} space`)),
    h("div", { class: `stats${insights === null ? "" : " five"}` },
      stat(summary.current, "Known now", () => showFacts("current")),
      stat(summary.past, "Replaced or ended", () => showFacts("past")),
      stat(waiting, "Waiting for review", () => selectTab("review"), waiting > 0),
      stat(summary.entities, "People and things", () => selectTab("map")),
      insights !== null && stat(insights, "Insights", () => showInsights("active"))),
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
    key === "insights" && insights > 0 && h("span", { class: "tab-count" }, insights),
    key === "review" && waiting > 0 && h("span", { class: "tab-badge" }, waiting)))));
}

function stat(value, label, onclick, attention) {
  return h("button", { class: `stat${attention ? " attention" : ""}`, onclick },
    h("div", { class: "stat-value" }, value.toLocaleString()),
    h("div", { class: "stat-label" }, label));
}

function showInsights(state) {
  S.insightState = state;
  S.tab = "insights";
  S.ask = null;
  S.insights = null;
  render();
  return guarded(async () => { await loadInsights(); render(); });
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
  const views = { facts: renderFacts, map: renderMap, timeline: renderTimeline, pages: renderPages, insights: renderInsights, review: renderReview, notes: renderNotes };
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

// The map is a live force layout like Obsidian's graph view: d3-force (Barnes-Hut repulsion) moves
// the nodes, a canvas draws them, and the simulation cools down and stops by itself. Nodes linked by
// facts pull together near the centre; nodes with no link sit on a loose ring around them.

let forceLoad = null;
let mapTeardown = null;

function loadForce() {
  forceLoad ??= new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = "/vendor/d3-force.min.js";
    script.onload = () => resolve(window.d3);
    script.onerror = () => { forceLoad = null; reject(new Error("could not load the map layout")); };
    document.head.append(script);
  });
  return forceLoad;
}

function buildGraph(facts) {
  // A refresh of the same scope keeps where the nodes already are, so the map does not jump.
  const previous = new Map((S.graph?.nodes || []).map((node) => [node.id, node]));
  const nodes = new Map();
  const touch = (id, name, kind) => {
    if (!nodes.has(id)) nodes.set(id, { id, name, kind, x: NaN, y: NaN, degree: 0, facts: [], neighbors: new Set() });
    return nodes.get(id);
  };
  const edges = [];
  const links = new Map();
  for (const fact of facts) {
    const from = touch(fact.subject_id, fact.subject_name, fact.subject_kind);
    from.facts.push(fact);
    if (!fact.object_id) continue;
    const to = touch(fact.object_id, fact.object_name, fact.object_kind);
    if (to === from) continue;
    to.facts.push(fact);
    from.degree++;
    to.degree++;
    from.neighbors.add(to);
    to.neighbors.add(from);
    edges.push({ fact, from, to });
    const key = from.id < to.id ? `${from.id} ${to.id}` : `${to.id} ${from.id}`;
    if (!links.has(key)) links.set(key, { source: from, target: to, facts: [] });
    links.get(key).facts.push(fact);
  }
  const list = [...nodes.values()];
  const ring = ringRadius(list);
  let kept = 0;
  let linked = 0;
  let alone = 0;
  const golden = Math.PI * (3 - Math.sqrt(5));
  for (const node of list) {
    const old = previous.get(node.id);
    if (old && Number.isFinite(old.x)) {
      node.x = old.x;
      node.y = old.y;
      kept++;
    } else if (node.degree) {
      const radius = 12 * Math.sqrt(++linked);
      node.x = radius * Math.cos(linked * golden);
      node.y = radius * Math.sin(linked * golden);
    } else {
      const angle = ++alone * golden;
      node.x = ring * Math.cos(angle);
      node.y = ring * Math.sin(angle);
    }
  }
  const kinds = [...new Set(list.map((node) => node.kind))];
  return {
    nodes: list,
    edges,
    links: [...links.values()],
    kinds,
    ring,
    total: facts.length,
    limited: facts.length >= GRAPH_LIMIT,
    // The same scope keeps its zoom and position too.
    view: previous.size ? S.graph.view : null,
    // How hot the layout still is: 1 is a fresh start, 0 is settled.
    alpha: kept === list.length ? 0 : kept ? 0.4 : 1,
    stats: { started: 0, settledMs: null, ticks: 0, draws: 0, drawMs: 0, maxDrawMs: 0 },
  };
}

// The ring for unlinked nodes: outside the linked clusters, and long enough to hold every one.
function ringRadius(nodes) {
  const linked = nodes.filter((node) => node.degree).length;
  const alone = nodes.length - linked;
  return Math.max(110, 24 * Math.sqrt(linked) + 60, (alone * 28) / (2 * Math.PI));
}

const nodeRadius = (node) => 5 + 2.6 * Math.sqrt(node.degree);
const shorten = (text, size) => (text.length > size ? `${text.slice(0, size - 1)}…` : text);
// Labels fade in as the map zooms in. A node with more links gets its label sooner.
const labelAlpha = (node, k) => Math.max(0, Math.min(1, (k * (1 + 0.6 * Math.log2(1 + node.degree)) - 0.95) / 0.35));

function mapColors() {
  const style = getComputedStyle(document.documentElement);
  const get = (name) => style.getPropertyValue(name).trim();
  return {
    nodes: [1, 2, 3, 4, 5, 6].map((i) => get(`--node-${i}`)),
    bg: get("--bg"),
    line: get("--line-strong"),
    accent: get("--accent"),
    accentInk: get("--accent-ink"),
    ink: get("--ink"),
    ink2: get("--ink-2"),
    font: getComputedStyle(document.body).fontFamily,
  };
}

// Eases `value` toward `target` with a time constant, so a fade or a zoom takes the same time at
// any frame rate. 50 ms covers 95% of the way in 150 ms.
const approach = (value, target, dt, tau) => {
  const next = value + (target - value) * (1 - Math.exp(-dt / tau));
  return Math.abs(target - next) < 0.004 ? target : next;
};

function renderMap() {
  const graph = S.graph;
  if (!graph) return loading();
  if (!graph.nodes.length) return empty("Nothing to draw yet", "The map shows the people and things in the facts known now.");
  if (!window.d3?.forceSimulation) {
    loadForce().then(
      () => { if (S.graph === graph && S.tab === "map" && !S.ask) renderBody(); },
      (error) => toast(error.message, true));
    return loading();
  }
  mapTeardown?.();

  const d3 = window.d3;
  const canvas = h("canvas", { class: "map", role: "img", "aria-label": `Map of ${graph.nodes.length} people and things` });
  const ctx = canvas.getContext("2d");
  const view = graph.view || { x: 0, y: 0, k: 1 };
  let autoFit = !graph.view;
  let colors = null;
  let hover = null;
  let hoverLink = null;
  let keyFocus = null;
  let width = 0;
  let height = 0;
  let frame = 0;
  let last = 0;
  // Where a zoom is heading: the world point under `cx, cy` stays put while `k` eases to its target.
  let zoomTo = null;
  // Where "Fit to window" is heading.
  let viewTo = null;
  // Pan speed in px per ms after the pointer lets go.
  let glide = null;

  const linked = graph.nodes.some((node) => node.degree);
  const alone = graph.nodes.some((node) => !node.degree);
  const ringed = (node) => linked && alone && !node.degree;
  // The simulation is stepped by the draw loop below, one tick per frame, never by its own timer.
  const sim = d3.forceSimulation(graph.nodes)
    .stop()
    .alpha(graph.alpha)
    .alphaDecay(0.03)
    .velocityDecay(0.4)
    .force("link", d3.forceLink(graph.links).distance((link) => 38 + 4 * Math.sqrt(link.source.degree + link.target.degree)))
    .force("charge", d3.forceManyBody().strength((node) => -70 - 30 * Math.sqrt(node.degree)).theta(0.9).distanceMax(500))
    .force("x", d3.forceX(0).strength((node) => (ringed(node) ? 0 : 0.07)))
    .force("y", d3.forceY(0).strength((node) => (ringed(node) ? 0 : 0.07)))
    .force("ring", d3.forceRadial(graph.ring).strength((node) => (ringed(node) ? 0.1 : 0)))
    .force("collide", d3.forceCollide((node) => nodeRadius(node) + 4).strength(0.7));
  const hot = () => sim.alpha() >= sim.alphaMin() || sim.alphaTarget() > 0;
  if (hot() && !graph.stats.started) {
    graph.stats.started = performance.now();
    // A few ticks before the first paint, so the map opens already spread out.
    sim.tick(30);
    graph.stats.ticks += 30;
    graph.alpha = sim.alpha();
  }

  // Leaving the tab or drawing the map again stops the loop and the layout: nothing runs in the background.
  function teardown() {
    graph.alpha = sim.alpha();
    cancelAnimationFrame(frame);
    frame = 0;
    resize.disconnect();
    theme.disconnect();
    dark.removeEventListener("change", recolor);
    if (mapTeardown === teardown) mapTeardown = null;
  }
  mapTeardown = teardown;

  function recolor() {
    if (!canvas.isConnected) { teardown(); return; }
    colors = null;
    wake();
  }
  const theme = new MutationObserver(recolor);
  theme.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
  const dark = matchMedia("(prefers-color-scheme: dark)");
  dark.addEventListener("change", recolor);

  const resize = new ResizeObserver(() => {
    if (!canvas.isConnected) { teardown(); return; }
    const box = canvas.getBoundingClientRect();
    const ratio = devicePixelRatio || 1;
    width = box.width;
    height = box.height;
    if (!width || !height) return;
    canvas.width = Math.round(width * ratio);
    canvas.height = Math.round(height * ratio);
    const before = graph.viewSize;
    if (autoFit) {
      Object.assign(view, fitView());
    } else if (before && (before.width !== width || before.height !== height)) {
      // A new size (a turned phone, a narrower window) keeps the same middle and the same zoom
      // relative to a fitted map.
      const scale = Math.min(width / before.width, height / before.height);
      const cx = (before.width / 2 - view.x) / view.k;
      const cy = (before.height / 2 - view.y) / view.k;
      view.k = Math.max(0.1, Math.min(4, view.k * scale));
      view.x = width / 2 - cx * view.k;
      view.y = height / 2 - cy * view.k;
    }
    graph.view = view;
    graph.viewSize = { width, height };
    step(performance.now());
  });
  resize.observe(canvas);

  // One requestAnimationFrame loop does everything: a layout tick, the view easing, the fades and the
  // draw. It runs only while something moves, and `wake` starts it again.
  function wake() {
    if (!frame && canvas.isConnected) {
      last = 0;
      frame = requestAnimationFrame(step);
    }
  }

  function step(now) {
    frame = 0;
    if (!canvas.isConnected) { teardown(); return; }
    if (!width) return;
    const started = performance.now();
    const dt = last ? Math.min(now - last, 64) : 16;
    last = now;
    let moving = false;

    if (hot()) {
      sim.tick();
      graph.stats.ticks++;
      graph.alpha = sim.alpha();
      if (hot()) moving = true;
      else settle();
      if (autoFit) {
        const goal = fitView();
        view.k += (goal.k - view.k) * 0.15;
        view.x += (goal.x - view.x) * 0.15;
        view.y += (goal.y - view.y) * 0.15;
      }
    }
    if (zoomTo) {
      view.k = approach(view.k, zoomTo.k, dt, 70);
      view.x = zoomTo.cx - zoomTo.wx * view.k;
      view.y = zoomTo.cy - zoomTo.wy * view.k;
      if (view.k === zoomTo.k) zoomTo = null;
      else moving = true;
    }
    if (viewTo) {
      for (const key of ["x", "y", "k"]) view[key] = approach(view[key], viewTo[key], dt, 70);
      if (view.k === viewTo.k && Math.abs(view.x - viewTo.x) < 0.5 && Math.abs(view.y - viewTo.y) < 0.5) viewTo = null;
      else moving = true;
    }
    if (glide) {
      view.x += glide.vx * dt;
      view.y += glide.vy * dt;
      const keep = Math.exp(-dt / 220);
      glide.vx *= keep;
      glide.vy *= keep;
      if (Math.hypot(glide.vx, glide.vy) < 0.02) glide = null;
      else moving = true;
    }
    graph.view = view;
    if (draw(dt)) moving = true;

    const stats = graph.stats;
    const spent = performance.now() - started;
    stats.draws++;
    stats.drawMs += spent;
    stats.maxDrawMs = Math.max(stats.maxDrawMs, spent);
    if (moving || drag) frame = requestAnimationFrame(step);
  }

  function settle() {
    graph.alpha = 0;
    if (graph.stats.started && graph.stats.settledMs === null) graph.stats.settledMs = Math.round(performance.now() - graph.stats.started);
    if (autoFit) viewTo = fitView();
  }

  const focused = () => hover || keyFocus || graph.nodes.find((node) => node.id === S.selectedNode) || null;

  // Draws one frame and moves every fade a step. Returns true while a fade is still on its way.
  function draw(dt) {
    colors ??= mapColors();
    const ratio = canvas.width / width;
    const focus = focused();
    const near = new Set(focus ? [focus, ...focus.neighbors] : []);
    const { k } = view;
    let fading = false;
    const ease = (value, target) => {
      const next = approach(value ?? target, target, dt, 50);
      if (next !== target) fading = true;
      return next;
    };

    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.clearRect(0, 0, width, height);
    ctx.setTransform(ratio * k, 0, 0, ratio * k, ratio * view.x, ratio * view.y);
    ctx.lineCap = "round";

    // Links: faded unless they touch the focused node, accent coloured when they do.
    const thin = Math.max(1.1, 0.7 / k);
    const thick = Math.max(1.8, 1.1 / k);
    for (const link of graph.links) {
      const on = link === hoverLink || (focus && (link.source === focus || link.target === focus));
      link.fade = ease(link.fade, focus && !on ? 0.12 : 1);
      link.glow = ease(link.glow, on ? 1 : 0);
      ctx.globalAlpha = link.fade * (1 - link.glow);
      if (ctx.globalAlpha > 0.01) {
        ctx.strokeStyle = colors.line;
        ctx.lineWidth = thin;
        ctx.beginPath();
        ctx.moveTo(link.source.x, link.source.y);
        ctx.lineTo(link.target.x, link.target.y);
        ctx.stroke();
      }
      if (link.glow > 0.01) {
        ctx.globalAlpha = link.glow;
        ctx.strokeStyle = colors.accent;
        ctx.lineWidth = thick;
        ctx.beginPath();
        ctx.moveTo(link.source.x, link.source.y);
        ctx.lineTo(link.target.x, link.target.y);
        ctx.stroke();
      }
    }

    for (const node of graph.nodes) {
      node.fade = ease(node.fade, focus && !near.has(node) ? 0.18 : 1);
      ctx.globalAlpha = node.fade;
      ctx.fillStyle = colors.nodes[graph.kinds.indexOf(node.kind) % 6];
      ctx.beginPath();
      ctx.arc(node.x, node.y, nodeRadius(node), 0, 2 * Math.PI);
      ctx.fill();
      const ring = node === keyFocus ? colors.accent : node.id === S.selectedNode || node === hover ? colors.ink : null;
      if (ring) {
        ctx.lineWidth = 2 / k;
        ctx.strokeStyle = ring;
        ctx.stroke();
      }
    }
    ctx.globalAlpha = 1;
    drawLabels(focus, near, ease);
    return fading;
  }

  // Labels are drawn on screen at a fixed size: the focused node and its neighbours first, then the
  // best linked nodes. A label that would overlap one already placed fades out instead.
  function drawLabels(focus, near, ease) {
    const { k } = view;
    const ratio = canvas.width / width;
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    ctx.textAlign = "center";
    ctx.textBaseline = "top";
    ctx.lineJoin = "round";
    ctx.lineWidth = 3.5;
    ctx.strokeStyle = colors.bg;
    const placed = [];
    const fits = (box) => !placed.some((o) => box.x < o.x + o.w && o.x < box.x + box.w && box.y < o.y + o.h && o.y < box.y + box.h);
    const label = (item, text, x, y, want, color, bold) => {
      ctx.font = `${bold ? "600 " : ""}12px ${colors.font}`;
      // Measuring text is the slow part of a frame, so each width is measured once per font.
      const key = `${ctx.font} ${text}`;
      if (item.labelKey !== key) {
        item.labelKey = key;
        item.labelWidth = ctx.measureText(text).width + 6;
      }
      const w = item.labelWidth;
      const box = { x: x - w / 2, y: y - 1, w, h: 16 };
      const seen = box.x < width && box.x + w > 0 && box.y < height && box.y + 16 > 0;
      const shown = want > 0.05 && seen && fits(box);
      if (shown) placed.push(box);
      item.labelFade = ease(item.labelFade ?? 0, shown ? want : 0);
      if (item.labelFade < 0.02 || !seen) return;
      ctx.globalAlpha = item.labelFade;
      ctx.strokeText(text, x, y);
      ctx.fillStyle = color;
      ctx.fillText(text, x, y);
    };
    const order = [...graph.nodes].sort((a, b) => (near.has(b) - near.has(a)) || (b.degree - a.degree) || (b.facts.length - a.facts.length));
    for (const node of order) {
      const forced = near.has(node);
      const want = forced ? 1 : labelAlpha(node, k) * (focus ? 0.25 : 1);
      const x = view.x + node.x * k;
      const y = view.y + (node.y + nodeRadius(node)) * k + 4;
      label(node, shorten(node.name, 28), x, y, want, forced || !focus ? colors.ink : colors.ink2, node === focus);
    }
    for (const link of graph.links) {
      const on = focus && (link.source === focus || link.target === focus);
      if (!on && !link.labelFade) continue;
      const more = link.facts.length > 1 ? ` +${link.facts.length - 1}` : "";
      const x = view.x + ((link.source.x + link.target.x) / 2) * k;
      const y = view.y + ((link.source.y + link.target.y) / 2) * k - 8;
      label(link, `${humanize(link.facts[0].predicate)}${more}`, x, y, on ? 1 : 0, colors.accentInk, false);
    }
    ctx.globalAlpha = 1;
  }

  // The view that frames every node.
  function fitView() {
    let [minX, maxX, minY, maxY] = [Infinity, -Infinity, Infinity, -Infinity];
    for (const node of graph.nodes) {
      const r = nodeRadius(node);
      minX = Math.min(minX, node.x - r);
      maxX = Math.max(maxX, node.x + r);
      minY = Math.min(minY, node.y - r);
      maxY = Math.max(maxY, node.y + r);
    }
    const pad = Math.min(60, width * 0.08);
    const k = Math.max(0.1, Math.min(1.6, (width - 2 * pad) / Math.max(maxX - minX, 1), (height - 2 * pad - 40) / Math.max(maxY - minY, 1)));
    return { k, x: width / 2 - ((minX + maxX) / 2) * k, y: height / 2 + 10 - ((minY + maxY) / 2) * k };
  }

  function stopViewMotion() {
    autoFit = false;
    viewTo = null;
    glide = null;
  }

  // Zooms toward a screen point, eased. Further wheel steps add to the zoom still under way.
  function zoom(factor, cx = width / 2, cy = height / 2) {
    stopViewMotion();
    const target = Math.max(0.1, Math.min(4, (zoomTo?.k ?? view.k) * factor));
    zoomTo = { cx, cy, wx: (cx - view.x) / view.k, wy: (cy - view.y) / view.k, k: target };
    wake();
  }

  function fit() {
    stopViewMotion();
    zoomTo = null;
    viewTo = fitView();
    wake();
  }

  const toWorld = (event) => {
    const box = canvas.getBoundingClientRect();
    const sx = event.clientX - box.left;
    const sy = event.clientY - box.top;
    return { sx, sy, x: (sx - view.x) / view.k, y: (sy - view.y) / view.k };
  };

  function nodeAt({ x, y }) {
    let best = null;
    let bestDistance = Infinity;
    for (const node of graph.nodes) {
      const distance = Math.hypot(node.x - x, node.y - y);
      if (distance < nodeRadius(node) + 5 / view.k && distance < bestDistance) {
        best = node;
        bestDistance = distance;
      }
    }
    return best;
  }

  function linkAt({ x, y }) {
    const reach = 5 / view.k;
    for (const link of graph.links) {
      const { source: a, target: b } = link;
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const t = Math.max(0, Math.min(1, ((x - a.x) * dx + (y - a.y) * dy) / (dx * dx + dy * dy || 1)));
      if (Math.hypot(a.x + t * dx - x, a.y + t * dy - y) < reach) return link;
    }
    return null;
  }

  function point(node, link) {
    if (node === hover && link === hoverLink) return;
    hover = node;
    hoverLink = link;
    canvas.style.cursor = node || link ? "pointer" : "";
    canvas.title = node
      ? `${node.name}: ${node.facts.length} fact${node.facts.length === 1 ? "" : "s"}`
      : link ? link.facts.map((fact) => fact.statement).join("\n") : "";
    wake();
  }

  canvas.addEventListener("wheel", (event) => {
    event.preventDefault();
    const { sx, sy } = toWorld(event);
    // Pixel deltas from a trackpad, lines from a mouse wheel.
    const delta = event.deltaMode === 1 ? event.deltaY * 16 : event.deltaY;
    zoom(Math.exp(-Math.max(-120, Math.min(120, delta)) * 0.0018), sx, sy);
  }, { passive: false });

  let drag = null;
  canvas.addEventListener("pointerdown", (event) => {
    const at = toWorld(event);
    const node = nodeAt(at);
    glide = null;
    drag = { node, link: node ? null : linkAt(at), sx: event.clientX, sy: event.clientY, moved: false, vx: view.x, vy: view.y, speed: { x: 0, y: 0 }, t: event.timeStamp, px: event.clientX, py: event.clientY };
    canvas.setPointerCapture(event.pointerId);
  });
  canvas.addEventListener("pointermove", (event) => {
    if (!drag) {
      const at = toWorld(event);
      const node = nodeAt(at);
      point(node, node ? null : linkAt(at));
      return;
    }
    const dx = event.clientX - drag.sx;
    const dy = event.clientY - drag.sy;
    if (!drag.moved && Math.hypot(dx, dy) > 3) {
      drag.moved = true;
      stopViewMotion();
      zoomTo = null;
      if (drag.node) {
        hover = drag.node;
        if (!graph.stats.started) graph.stats.started = performance.now();
        sim.alphaTarget(0.3);
      } else {
        canvas.classList.add("panning");
      }
    }
    if (!drag.moved) return;
    if (drag.node) {
      const at = toWorld(event);
      drag.node.fx = at.x;
      drag.node.fy = at.y;
    } else {
      view.x = drag.vx + dx;
      view.y = drag.vy + dy;
      // Smoothed pointer speed, for the glide after release.
      const span = Math.max(event.timeStamp - drag.t, 1);
      drag.speed.x = drag.speed.x * 0.6 + ((event.clientX - drag.px) / span) * 0.4;
      drag.speed.y = drag.speed.y * 0.6 + ((event.clientY - drag.py) / span) * 0.4;
    }
    drag.t = event.timeStamp;
    drag.px = event.clientX;
    drag.py = event.clientY;
    wake();
  });
  const release = (event) => {
    if (!drag) return;
    canvas.classList.remove("panning");
    if (drag.moved && drag.node) {
      drag.node.fx = null;
      drag.node.fy = null;
      sim.alphaTarget(0);
    } else if (drag.moved) {
      // No glide when the pointer stopped before letting go.
      if (event.timeStamp - drag.t < 80) glide = { vx: drag.speed.x, vy: drag.speed.y };
    } else if (drag.node) {
      selectNode(drag.node);
    } else if (drag.link) {
      openFact(drag.link.facts[0].id);
    } else if (S.selectedNode) {
      closeSelection();
    }
    drag = null;
    wake();
  };
  canvas.addEventListener("pointerup", release);
  canvas.addEventListener("pointercancel", release);
  canvas.addEventListener("pointerleave", () => { if (!drag) point(null, null); });

  function selectNode(node) {
    S.selectedNode = node.id;
    S.drawer = { type: "entity", node };
    renderDrawer();
    wake();
  }
  function closeSelection() {
    S.selectedNode = null;
    S.drawer = null;
    renderDrawer();
    wake();
  }

  // The canvas is one picture to a screen reader. These buttons, hidden until focused, give each
  // node a name, a place in the tab order, and Enter to open it.
  const keys = h("div", { class: "map-keys", "aria-label": "People and things on the map" },
    [...graph.nodes].sort((a, b) => b.degree - a.degree || a.name.localeCompare(b.name)).map((node) => h("button", {
      "aria-label": `${node.name}, ${node.facts.length} fact${node.facts.length === 1 ? "" : "s"}`,
      onfocus: () => { keyFocus = node; bringIntoView(node); wake(); },
      onblur: () => { if (keyFocus === node) keyFocus = null; wake(); },
      onclick: () => selectNode(node),
    }, node.name)));

  function bringIntoView(node) {
    const x = view.x + node.x * view.k;
    const y = view.y + node.y * view.k;
    if (x > 40 && x < width - 40 && y > 40 && y < height - 40) return;
    stopViewMotion();
    zoomTo = null;
    viewTo = { k: view.k, x: width / 2 - node.x * view.k, y: height / 2 - node.y * view.k };
  }

  return h("div", {},
    canvas,
    keys,
    h("div", { class: "map-legend" }, graph.kinds.slice(0, 6).map((kind, index) => h("span", { class: "legend-item" },
      h("span", { class: "legend-swatch", css: { background: `var(--node-${index + 1})` } }), humanize(kind)))),
    h("div", { class: "map-note" }, graph.limited ? `Newest ${GRAPH_LIMIT} facts. Click anything for detail.` : "Click anything for detail. Drag to move, scroll to zoom."),
    h("div", { class: "map-controls" },
      h("button", { "aria-label": "Zoom in", title: "Zoom in", onclick: () => zoom(1.25) }, "+"),
      h("button", { "aria-label": "Zoom out", title: "Zoom out", onclick: () => zoom(0.8) }, "−"),
      h("button", { "aria-label": "Fit to window", title: "Fit to window", onclick: fit }, "⤢")));
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

// ---- Insights ---------------------------------------------------------------------------------

// Highest level first: what to do, what it means, who or what this is.
const LEVEL = {
  guidance: ["Guidance", "What to do about it"],
  understanding: ["Understanding", "What it means"],
  context: ["Context", "Who or what this is"],
};

const INSIGHT_STATUS = {
  active: ["Current", "current", "Your agent is shown this."],
  stale: ["Stale", "waiting", "Something it was built on changed. Your agent is not shown it until it looks again."],
  quarantined: ["Waiting for review", "waiting", "Held back until you approve it in Review."],
  rejected: ["Rejected", "bad", "You rejected it. Your agent is never shown it."],
  retired: ["Retired", "past", "Withdrawn as no longer true."],
  superseded: ["Replaced", "past", "A newer insight took its place."],
};

const insightStatus = (status) => INSIGHT_STATUS[status] || [humanize(status), "past", ""];

// What happened to one source of a stale insight.
const CHANGE = {
  superseded: "Replaced by a newer fact",
  invalidated: "Retracted",
  expired: "Expired",
  purged: "Removed or lowered in trust by a purge",
  insight_changed: "The insight it builds on changed",
};

function changeText(change) {
  if (change.change === "insight_changed" && change.status) {
    return `The insight it builds on is now ${insightStatus(change.status)[0].toLowerCase()}`;
  }
  return CHANGE[change.change] || humanize(change.change);
}

function renderInsights() {
  if (S.insights === null) return loading();
  if (S.insightsMissing) {
    return empty("Your server is older than this page", "Insights need a newer server. Update it with: docker compose -f ~/.config/memoryos/docker-compose.yml up -d --build");
  }
  const count = S.insights.length;
  const toolbar = h("div", { class: "toolbar" },
    h("div", { class: "segmented", role: "group", "aria-label": "Which insights" },
      [["active", "Current"], ["stale", "Stale"], ["all", "All"]].map(([key, label]) => h("button", {
        "aria-pressed": S.insightState === key ? "true" : "false",
        onclick: () => showInsights(key),
      }, label))),
    h("span", { class: "toolbar-note" }, count >= INSIGHT_LIMIT ? `Newest ${INSIGHT_LIMIT}` : `${count} insight${count === 1 ? "" : "s"}`));
  if (!count) {
    const words = {
      active: ["No insights yet", "When your agent works out what several facts mean together, it saves an insight. They show up here."],
      stale: ["Nothing is stale", "An insight turns stale when a fact it was built on changes. None has."],
      all: ["No insights yet", "When your agent works out what several facts mean together, it saves an insight. They show up here."],
    }[S.insightState];
    return h("div", {}, toolbar, empty(...words));
  }
  return h("div", {}, toolbar, Object.entries(LEVEL).map(([level, [label, help]]) => {
    const items = S.insights.filter((insight) => insight.level === level);
    if (!items.length) return null;
    return h("section", { class: "group" },
      h("div", { class: "group-title" }, label, h("span", { class: "kind-tag" }, `${help}, ${items.length}`)),
      h("div", { class: "list" }, items.map(insightRow)));
  }));
}

function changeSummary(reasons) {
  const counts = new Map();
  for (const reason of reasons || []) counts.set(reason.change, (counts.get(reason.change) || 0) + 1);
  const words = { superseded: "replaced", invalidated: "retracted", expired: "expired", purged: "purged", insight_changed: "changed" };
  return [...counts].map(([change, n]) => `${n} source${n === 1 ? "" : "s"} ${words[change] || humanize(change)}`).join(", ");
}

function insightRow(insight) {
  const [label, kind] = insightStatus(insight.status);
  const ended = ["rejected", "retired", "superseded"].includes(insight.status);
  return h("button", {
    class: `row${ended ? " is-past" : ""}`,
    "aria-current": S.drawer?.id === insight.id ? "true" : null,
    onclick: () => openInsight(insight.id),
  },
    h("div", { class: "row-text" }, insight.statement),
    h("div", { class: "meta" },
      insight.status !== "active" && h("span", { class: `pill ${kind}` }, label),
      insight.status === "stale" && h("span", { class: "change" }, changeSummary(insight.stale_reason)),
      h("span", {}, `${insight.sources.length} source${insight.sources.length === 1 ? "" : "s"}`),
      h("span", {}, `since ${date(insight.valid_from)}`),
      h("span", {}, TRUST[insight.trust]?.[0] || insight.trust)));
}

// ---- Review -----------------------------------------------------------------------------------

function renderReview() {
  if (S.review === null || S.pageReview === null || S.insightReview === null) return loading();
  if (S.reviewDenied) return empty("Review needs the admin key", "This page is using a key that cannot review. Run memoryos setup to create the admin key.");
  if (!S.review.length && !S.pageReview.length && !S.insightReview.length) {
    return empty("All clear", "Nothing is waiting. Facts, page edits and insights held back from your agent's memory will show up here for you to decide.");
  }
  const kinds = [S.review, S.pageReview, S.insightReview].filter((list) => list.length > 0).length;
  const titled = kinds > 1;
  return h("div", {},
    h("div", { class: "toolbar" }, h("span", { class: "toolbar-note", css: { "margin-left": "0" } },
      "Your agent cannot recall these until you approve them. Check the source: outside content can try to plant false facts.")),
    S.pageReview.length > 0 && h("section", { class: "group" },
      titled && h("div", { class: "group-title" }, "Page edits", h("span", { class: "kind-tag" }, S.pageReview.length)),
      S.pageReview.map(renderPageEdit)),
    S.insightReview.length > 0 && h("section", { class: "group" },
      titled && h("div", { class: "group-title" }, "Insights", h("span", { class: "kind-tag" }, S.insightReview.length)),
      S.insightReview.map(renderInsightReview)),
    S.review.length > 0 && h("section", { class: "group" },
      titled && h("div", { class: "group-title" }, "Facts", h("span", { class: "kind-tag" }, S.review.length)),
      renderFactReview()));
}

function renderInsightReview({ insight, sources, replaces }) {
  return h("article", { class: "card" },
    h("div", { class: "card-title" }, insight.statement),
    h("div", { class: "meta" },
      h("span", {}, LEVEL[insight.level]?.[0] || insight.level),
      h("span", {}, TRUST[insight.trust]?.[0] || insight.trust),
      h("span", {}, `by ${insight.written_by}`),
      h("span", {}, `added ${ago(insight.recorded_at)}`)),
    replaces && h("div", { class: "quote" }, h("span", { class: "quote-label" }, "Would replace"), replaces.statement),
    h("div", { class: "quote" }, h("span", { class: "quote-label" }, `Built on ${insight.sources.length} source${insight.sources.length === 1 ? "" : "s"}`),
      sources ? h("ul", { class: "source-quotes" }, sources.map((view) => h("li", {}, view.statement ?? "Deleted by a purge")))
        : "The sources could not be read."),
    h("div", { class: "actions" },
      h("button", { class: "btn primary", onclick: (event) => { event.target.disabled = true; decideInsight(insight.id, "approve"); } }, "Approve"),
      h("button", { class: "btn danger", onclick: (event) => { event.target.disabled = true; decideInsight(insight.id, "reject"); } }, "Reject"),
      h("button", { class: "link-btn", css: { "margin-left": "auto" }, onclick: () => openInsight(insight.id) }, "Details")));
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

// "New page": a name, then the editor opens on an empty page; saving creates it.
function renderNewPage() {
  const scope = S.scope;
  const start = () => openDialog({
    title: "New page",
    intro: `In ${scope}. Your editor opens on an empty page; save and close it to create the page.`,
    fields: [
      { name: "page", label: "Page name", placeholder: "deploy-guide", hint: "Lowercase letters, digits, '.', '_' and '-'. The title goes at the top of the page.",
        check: PAGE_NAME, problem: "Use lowercase letters, digits, '.', '_' and '-', such as deploy-guide." },
    ],
    submit: "Open in editor",
    onSubmit: (slug) => editInEditor(slug, scope),
  });
  return h("button", { class: "btn", onclick: start }, "New page");
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
  const { type } = S.drawer;
  const content = type === "entity" ? renderEntityDrawer(S.drawer.node)
    : type === "insight" ? renderInsightDrawer(S.drawer) : renderFactDrawer(S.drawer);
  document.body.append(h("aside", { class: "drawer", id: "drawer", "aria-label": "Detail" },
    h("div", { class: "drawer-head" },
      h("strong", {}, type === "entity" ? humanize(S.drawer.node.kind) : type === "insight" ? "Insight" : "Fact"),
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

const openSource = (kind, id) => (kind === "insight" ? openInsight(id) : openFact(id));

function renderInsightDrawer({ insight }) {
  if (!insight) return loading();
  const [label, kind, meaning] = insightStatus(insight.status);
  const level = LEVEL[insight.level] || [humanize(insight.level), ""];
  const trust = TRUST[insight.trust] || [insight.trust, ""];
  const changes = new Map((insight.stale_reason || []).map((change) => [change.id, change]));
  const views = insight.source_views || [];
  const byId = new Map(views.map((view) => [view.id, view]));
  const history = insight.history || [];
  return h("div", {},
    h("div", { class: "drawer-title" }, insight.statement),
    h("div", { class: "meta" }, h("span", { class: `pill ${kind}` }, label), h("span", {}, level[0])),
    h("dl", { class: "fields" },
      h("dt", {}, "Level"), h("dd", {}, level[0], h("div", { class: "help" }, level[1])),
      h("dt", {}, "Status"), h("dd", {}, label, meaning && h("div", { class: "help" }, meaning)),
      h("dt", {}, "True from"), h("dd", {}, date(insight.valid_from)),
      insight.valid_to && [h("dt", {}, "Until"), h("dd", {}, date(insight.valid_to))],
      h("dt", {}, "Learned"), h("dd", {}, `${date(insight.recorded_at)} (${ago(insight.recorded_at)})`),
      h("dt", {}, "Written by"), h("dd", {}, insight.written_by),
      h("dt", {}, "Trust"), h("dd", {}, trust[0], trust[1] && h("div", { class: "help" }, trust[1])),
      h("dt", {}, "Space"), h("dd", {}, insight.scope)),
    insight.superseded_by && h("div", { class: "section" },
      h("button", { class: "link-btn", onclick: () => openInsight(insight.superseded_by) }, "Read the insight that replaced it")),
    insight.pending_supersede_of && h("div", { class: "section" },
      h("button", { class: "link-btn", onclick: () => openInsight(insight.pending_supersede_of) }, "Read the insight it would replace")),
    changes.size > 0 && h("div", { class: "section" }, h("h3", {}, "What changed"),
      [...changes.values()].map((change) => h("div", { class: "change-item" },
        h("div", { class: "change" }, changeText(change)),
        h("div", { class: "chain-text" }, byId.get(change.id)?.statement ?? "Deleted by a purge"),
        change.by && h("button", { class: "link-btn", onclick: () => openFact(change.by) }, "Read the newer fact")))),
    h("div", { class: "section" }, h("h3", {}, `Built on ${views.length} source${views.length === 1 ? "" : "s"}`),
      views.map(renderSourceView)),
    history.length > 0 && h("div", { class: "section" }, h("h3", {}, "Insights it replaced"),
      h("div", { class: "chain" }, history.map((item) => h("button", { class: "chain-item", onclick: () => openInsight(item.id) },
        h("div", { class: "chain-text" }, item.statement),
        h("div", { class: "meta" }, `${date(item.valid_from)} to ${item.valid_to ? date(item.valid_to) : "now"}`))))),
    insight.status === "quarantined" && h("div", { class: "actions" },
      h("button", { class: "btn primary", onclick: (event) => { event.target.disabled = true; decideInsight(insight.id, "approve"); } }, "Approve"),
      h("button", { class: "btn danger", onclick: (event) => { event.target.disabled = true; decideInsight(insight.id, "reject"); } }, "Reject")),
    (insight.status === "active" || insight.status === "stale") && h("div", { class: "actions" },
      h("button", { class: "btn danger", onclick: () => retireInsight(insight) }, "Retire")));
}

// One source as it is now: a fact or a lower insight, which opens in this drawer. What changed is
// shown once, above, under "What changed".
function renderSourceView(view) {
  const content = [
    h("div", { class: "source-top" },
      h("span", { class: "kind-tag" }, view.kind === "insight" ? "Insight" : "Fact"),
      h("span", { class: `pill ${view.current ? "current" : "past"}` }, view.current ? "Current" : "No longer current")),
    h("div", { class: "chain-text" }, view.statement ?? "Deleted by a purge"),
  ];
  // A purged source has nothing left to open.
  if (view.statement === null || view.statement === undefined) return h("div", { class: "mini-row source-row" }, content);
  return h("button", { class: "mini-row source-row", onclick: () => openSource(view.kind, view.id) }, content);
}

// ---- Start ------------------------------------------------------------------------------------

function toggleTheme() {
  const dark = document.documentElement.dataset.theme === "dark"
    || (!document.documentElement.dataset.theme && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.dataset.theme = dark ? "light" : "dark";
  storageSet(localStorage, "memoryos-theme", document.documentElement.dataset.theme);
}

document.addEventListener("keydown", (event) => {
  // Esc in an open dialog closes only the dialog.
  if (document.querySelector("dialog[open]")) return;
  if (event.key === "Escape" && S.drawer) { closeDrawer(); return; }
  const typing = /^(INPUT|SELECT|TEXTAREA)$/.test(event.target.tagName);
  if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
  if (event.key === "/") { event.preventDefault(); document.getElementById("ask")?.focus(); }
  const tabs = { 1: "facts", 2: "map", 3: "timeline", 4: "review", 5: "insights" };
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
    S.signature = countsSignature();
    render();
    await loadTab();
    render();
  });
  setInterval(() => { if (!document.hidden) refresh(true); }, REFRESH_MS);
  addEventListener("focus", () => refresh(true));
}

start();
