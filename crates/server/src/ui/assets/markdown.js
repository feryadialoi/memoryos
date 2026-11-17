"use strict";
// A small markdown renderer for knowledge pages. Page text is untrusted (agents write it), so this
// builds DOM nodes and sets all text as text: no HTML from the page is ever parsed. Raw HTML in the
// markdown shows as text. Links are kept only for http, https and mailto. Mermaid blocks become
// placeholders that `renderMermaid` fills in.

const Markdown = (() => {
  const el = (tag, props, ...children) => {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) node.setAttribute(key, value);
    for (const child of children.flat()) {
      if (child === null || child === undefined || child === false) continue;
      node.append(typeof child === "string" ? document.createTextNode(child) : child);
    }
    return node;
  };

  const SAFE_URL = /^(https?:\/\/|mailto:)/i;

  // Inline: code, links, bold, italic, strikethrough. Earliest match wins; code spans are literal.
  const INLINE = [
    ["code", /`([^`]+)`/],
    ["link", /!?\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/],
    ["strong", /\*\*([^*]+)\*\*|__([^_]+)__/],
    ["del", /~~([^~]+)~~/],
    ["em", /\*([^*\s][^*]*)\*|(?<![A-Za-z0-9])_([^_\s][^_]*)_(?![A-Za-z0-9])/],
    ["url", /\bhttps?:\/\/[^\s<>()]+[^\s<>().,;:!?'"]/],
  ];

  function inline(text) {
    const out = [];
    let rest = text;
    while (rest) {
      let best = null;
      for (const [kind, pattern] of INLINE) {
        const match = pattern.exec(rest);
        if (match && (!best || match.index < best.match.index)) best = { kind, match };
      }
      if (!best) {
        out.push(rest);
        break;
      }
      const { kind, match } = best;
      if (match.index > 0) out.push(rest.slice(0, match.index));
      const inner = match[1] ?? match[2] ?? "";
      if (kind === "code") out.push(el("code", {}, inner));
      else if (kind === "link") {
        const url = match[2];
        out.push(SAFE_URL.test(url)
          ? el("a", { href: url, target: "_blank", rel: "noopener noreferrer" }, inline(match[1] || url))
          : el("span", {}, inline(match[1] || url)));
      } else if (kind === "url") out.push(el("a", { href: match[0], target: "_blank", rel: "noopener noreferrer" }, match[0]));
      else out.push(el(kind, {}, inline(inner)));
      rest = rest.slice(match.index + match[0].length);
    }
    return out;
  }

  const FENCE = /^\s*(```+|~~~+)\s*([\w+-]*)/;
  const HEADING = /^(#{1,6})\s+(.*?)\s*#*\s*$/;
  const RULE = /^\s*([-*_])(\s*\1){2,}\s*$/;
  const ITEM = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
  const TABLE_SEP = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

  const cells = (line) => line.trim().replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());

  function blocks(lines) {
    const out = [];
    let i = 0;
    while (i < lines.length) {
      const line = lines[i];
      if (!line.trim()) { i++; continue; }

      const fence = FENCE.exec(line);
      if (fence) {
        const body = [];
        i++;
        while (i < lines.length && !lines[i].trim().startsWith(fence[1])) body.push(lines[i++]);
        i++;
        const source = body.join("\n");
        if (fence[2].toLowerCase() === "mermaid") {
          out.push(el("div", { class: "md-mermaid", "data-source": source }, el("pre", {}, el("code", {}, source))));
        } else {
          out.push(el("pre", { class: "md-code" }, el("code", fence[2] ? { "data-lang": fence[2] } : {}, source)));
        }
        continue;
      }

      const heading = HEADING.exec(line);
      if (heading) {
        out.push(el(`h${heading[1].length}`, { class: "md-h" }, inline(heading[2])));
        i++;
        continue;
      }

      if (RULE.test(line)) { out.push(el("hr", {})); i++; continue; }

      if (line.trimStart().startsWith(">")) {
        const quoted = [];
        while (i < lines.length && lines[i].trimStart().startsWith(">")) quoted.push(lines[i++].trimStart().replace(/^>\s?/, ""));
        out.push(el("blockquote", {}, blocks(quoted)));
        continue;
      }

      if (line.includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1]) && lines[i + 1].includes("-")) {
        const head = cells(line);
        i += 2;
        const rows = [];
        while (i < lines.length && lines[i].includes("|") && lines[i].trim()) rows.push(cells(lines[i++]));
        out.push(el("div", { class: "md-table" }, el("table", {},
          el("thead", {}, el("tr", {}, head.map((c) => el("th", {}, inline(c))))),
          el("tbody", {}, rows.map((row) => el("tr", {}, head.map((_, n) => el("td", {}, inline(row[n] || "")))))))));
        continue;
      }

      const item = ITEM.exec(line);
      if (item) {
        const [list, next] = listBlock(lines, i, item[1].length);
        out.push(list);
        i = next;
        continue;
      }

      const para = [];
      while (i < lines.length && lines[i].trim() && !FENCE.test(lines[i]) && !HEADING.test(lines[i])
        && !lines[i].trimStart().startsWith(">") && !ITEM.exec(lines[i]) && !RULE.test(lines[i])) {
        para.push(lines[i++].trim());
      }
      if (!para.length) para.push(lines[i++].trim());
      out.push(el("p", {}, inline(para.join(" "))));
    }
    return out;
  }

  // A list at `indent`; deeper items nest under the item before them.
  function listBlock(lines, start, indent) {
    const ordered = /\d/.test(ITEM.exec(lines[start])[2]);
    const list = el(ordered ? "ol" : "ul", {});
    let i = start;
    let current = null;
    while (i < lines.length) {
      const line = lines[i];
      if (!line.trim()) {
        const following = lines[i + 1] && ITEM.exec(lines[i + 1]);
        if (following && following[1].length >= indent) { i++; continue; }
        break;
      }
      const item = ITEM.exec(line);
      if (item && item[1].length === indent && /\d/.test(item[2]) === ordered) {
        let text = item[3];
        const task = /^\[([ xX])\]\s+(.*)$/.exec(text);
        current = el("li", task ? { class: "md-task" } : {});
        if (task) {
          const box = el("input", { type: "checkbox", disabled: "" });
          box.checked = task[1] !== " ";
          current.append(box, " ");
          text = task[2];
        }
        current.append(...inline(text));
        list.append(current);
        i++;
      } else if (item && item[1].length > indent && current) {
        const [nested, next] = listBlock(lines, i, item[1].length);
        current.append(nested);
        i = next;
      } else if (!item && current && /^\s+/.test(line)) {
        current.append(" ", ...inline(line.trim()));
        i++;
      } else {
        break;
      }
    }
    return [list, i];
  }

  function render(text) {
    return el("div", { class: "md" }, blocks(String(text).replace(/\r\n?/g, "\n").split("\n")));
  }

  let mermaidLoad = null;
  let diagramCount = 0;

  // Loads the bundled mermaid on first use, then draws each placeholder under `root`. A diagram that
  // fails to parse keeps its source and shows the error.
  async function renderMermaid(root, dark) {
    const pending = [...root.querySelectorAll(".md-mermaid:not([data-drawn])")];
    if (!pending.length) return;
    mermaidLoad ??= new Promise((resolve, reject) => {
      const script = document.createElement("script");
      script.src = "/vendor/mermaid.min.js";
      script.onload = () => resolve(window.mermaid);
      script.onerror = () => { mermaidLoad = null; reject(new Error("could not load the diagram renderer")); };
      document.head.append(script);
    });
    let mermaid;
    try {
      mermaid = await mermaidLoad;
    } catch (error) {
      for (const block of pending) block.append(el("div", { class: "md-error" }, error.message));
      return;
    }
    mermaid.initialize({ startOnLoad: false, securityLevel: "strict", theme: dark ? "dark" : "default" });
    for (const block of pending) {
      block.setAttribute("data-drawn", "");
      try {
        const { svg } = await mermaid.render(`mermaid-${++diagramCount}`, block.getAttribute("data-source"));
        const doc = new DOMParser().parseFromString(svg, "image/svg+xml");
        const drawn = doc.documentElement;
        if (drawn.nodeName !== "svg") throw new Error("the diagram did not render");
        block.replaceChildren(document.importNode(drawn, true));
      } catch (error) {
        block.append(el("div", { class: "md-error" }, `Diagram error: ${error?.message || error}`));
      }
    }
  }

  return { render, renderMermaid };
})();
