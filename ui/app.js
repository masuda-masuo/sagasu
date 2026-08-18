// sagasu browse screen — M4 step 3 frontend.
//
// Static files only: no build step, no bundler, no runtime network. Every data
// read happens in loadBrowseView(), the single function that step 2 replaces
// with a Tauri invoke() call; nothing else on this page changes when that
// swap happens.
//
// Layout of this file:
//   1. loadBrowseView()          — selection -> BrowseViewDto (the swap point)
//   2. render(view, selection)   — pure function of the DTO + the selection
//   3. tiny DOM helper           — el(), the only way nodes are built
//   4. wiring                    — clicks, crumbs, location.hash state
//
// Two render rules that are easy to break and load-bearing to keep:
//   - mtime_ns / ctime_ns arrive as decimal strings because Number would
//     round them (Number.MAX_SAFE_INTEGER is 9.0e15; a real mtime is ~1.75e18).
//     They are rendered as the strings they arrived as — never Number(),
//     parseInt() or arithmetic.
//   - magic / blake3 / fs_id arrive as lowercase hex and are displayed as-is.

"use strict";

// ---------------------------------------------------------------------------
// 1. loadBrowseView(selection) — the only data path
// ---------------------------------------------------------------------------

let fixtureMapPromise = null;

function loadFixtureMap() {
  if (fixtureMapPromise === null) {
    fixtureMapPromise = fetch("fixtures/index.json").then((res) => {
      if (!res.ok) {
        throw new Error(`fixtures/index.json — HTTP ${res.status}`);
      }
      return res.json();
    });
  }
  return fixtureMapPromise;
}

// Turn a selection (array of `namespace:value` strings) into a BrowseViewDto.
// Step 3: resolve ui/fixtures/index.json and fetch the fixture file.
// Step 2: this body becomes `invoke("browse_view", { query: { selected } })`
// and nothing else on the page changes.
//
// A selection with no fixture is an error with a name, not an empty view:
// the mock may have holes, but it must not lie about them.
async function loadBrowseView(selection) {
  const key = JSON.stringify(selection);
  let map;
  try {
    map = await loadFixtureMap();
  } catch (err) {
    throw new Error(
      `cannot load the fixture map: ${err.message} — serve ui/ over HTTP ` +
        `(cd ui && python3 -m http.server 8000); most browsers refuse fetch() ` +
        `on file:// URLs`
    );
  }
  const file = map[key];
  if (file === undefined) {
    const err = new Error(`no fixture for selection ${key}`);
    err.noFixture = true;
    err.known = Object.keys(map);
    throw err;
  }
  const res = await fetch(`fixtures/${file}`);
  if (!res.ok) {
    throw new Error(`fixtures/${file} — HTTP ${res.status}`);
  }
  return res.json();
}

// ---------------------------------------------------------------------------
// 2. render(view, selection) — the whole screen as a pure function
// ---------------------------------------------------------------------------

function render(view, selection) {
  renderSnapshot(view);
  renderSelection(view, selection);
  renderCounts(view);
  renderLabel(view);
  renderUniversal(view);
  renderRecommended(view);
  renderAxes(view);
  renderPreview(view);
  renderDropped(view);
}

function renderSnapshot(view) {
  const s = view.snapshot;
  let text;
  if (!s.built) {
    text =
      `No tag layer: this index has never been tagged (metadata index is at ` +
      `scan generation ${s.scan_generation}) — the axes above are empty for a ` +
      `reason. Run \`sagasu tag\`.`;
  } else if (s.behind > 0) {
    text =
      `Tag layer is a snapshot of crawl ${s.tag_scan_generation}; the metadata ` +
      `index has crawled ${fmt(s.behind)} more since — every file indexed ` +
      `after the snapshot is missing from the counts above.`;
  } else {
    text =
      `Tag layer is a snapshot of crawl ${s.tag_scan_generation}; the metadata ` +
      `index is at the same crawl — the counts above are current for that ` +
      `snapshot.`;
  }
  byId("snapshot").textContent = text;
}

function renderSelection(view, selection) {
  const crumbs = byId("crumbs");
  crumbs.textContent = "";

  const root = el("button", "crumb", "whole index");
  if (selection.length === 0) {
    root.disabled = true;
    root.classList.add("current");
  } else {
    root.addEventListener("click", () => go([]));
  }
  crumbs.appendChild(root);

  selection.forEach((tag, i) => {
    const crumb = el("button", "crumb", tag);
    const last = i === selection.length - 1;
    if (last) {
      crumb.disabled = true;
      crumb.classList.add("current");
    } else {
      // A crumb is a way back: clicking it drops everything after it.
      crumb.addEventListener("click", () => go(selection.slice(0, i + 1)));
    }
    crumbs.appendChild(crumb);
  });
}

function renderCounts(view) {
  const p = byId("counts");
  p.textContent = "";
  const share = view.corpus > 0 ? view.matched / view.corpus : 0;
  p.appendChild(
    el(
      "span",
      "counts-headline",
      `${fmt(view.matched)} of ${fmt(view.corpus)} live files (${pct(share)})`
    )
  );
  if (view.matched === 0) {
    p.appendChild(
      el("span", "note", " — no live file carries all of these tags")
    );
  }
  // Same footing as the CLI (docs/browse.md §6): these counts come from the
  // index and no row behind them was stat'd, so they are an upper bound.
  p.appendChild(
    el(
      "div",
      "note",
      "indexed count — an upper bound; only the previewed rows below were " +
        "checked against the filesystem"
    )
  );
}

function renderLabel(view) {
  const p = byId("label");
  p.textContent = "";
  if (view.label.length === 0) {
    // Three reasons for an empty label, and only one of them is "nothing to
    // say" — the same distinction the CLI makes (sagasu-cli/src/browse.rs).
    if (view.matched === 0) {
      p.textContent = "(none — this group is empty)";
    } else if (view.label_vocabulary === 0) {
      p.textContent =
        "(none — this group carries no tag beyond the selection)";
    } else {
      p.textContent =
        `(suppressed — ${fmt(view.label_vocabulary)} candidate tags were computed)`;
    }
    return;
  }
  for (const term of view.label) {
    const span = el("span", "label-term");
    span.appendChild(el("span", "label-tag", term.tag.tag));
    span.appendChild(
      el("span", "label-count", `(${fmt(term.files)}/${fmt(view.matched)})`)
    );
    p.appendChild(span);
  }
  // "5 of 5" and "5 of 900" are different amounts of trust in a label.
  const vocabNote =
    view.label_vocabulary > view.label.length
      ? `c-TF-IDF over ${fmt(view.label_vocabulary)} candidate tags — the ` +
        `${view.label.length} shown are the best`
      : `c-TF-IDF over ${fmt(view.label_vocabulary)} candidate tags`;
  p.appendChild(el("div", "note", vocabNote));
}

function renderUniversal(view) {
  const p = byId("universal");
  p.textContent = "";
  if (view.universal.length === 0) {
    p.textContent = "(none — no tag is carried by every file in this group)";
    return;
  }
  p.appendChild(el("span", null, "every file in this group carries "));
  view.universal.forEach((t, i) => {
    if (i > 0) p.appendChild(el("span", null, ", "));
    // Deliberately not a clickable step: choosing a universal value returns
    // the same group (sagasu-ui dto docs), so it is rendered as a fact.
    p.appendChild(el("span", "chip chip-static", t.tag));
  });
  p.appendChild(
    el(
      "span",
      "note",
      " — none of these narrows it. Choosing one returns the same group."
    )
  );
}

function renderRecommended(view) {
  const box = byId("recommended");
  box.textContent = "";
  if (view.recommended) {
    const step = view.recommended;
    // A card of its own, not an axis row: docs/browse.md §2-1 — the
    // recommended step is deliberately not the top row of the top axis, and
    // rendering it as just another row would destroy the one thing it carries.
    const card = el("button", "recommended-card");
    card.title = "click to take this step";
    card.appendChild(el("span", "recommended-tag", step.tag.tag));
    card.appendChild(
      el(
        "span",
        "recommended-meta",
        `${fmt(step.files)} of ${fmt(view.matched)} files remain (${pct(step.share)})`
      )
    );
    card.appendChild(
      el(
        "span",
        "recommended-bits",
        `${step.bits.toFixed(2)} bits — the step whose outcome is least predictable`
      )
    );
    card.addEventListener("click", () => drill(step.tag.tag));
    box.appendChild(card);
    box.appendChild(
      el(
        "span",
        "note",
        "deliberately not the top row of the top axis — click the card to " +
          "take this step."
      )
    );
  } else {
    // The four different reasons for no step, told apart the way the CLI
    // tells them apart (sagasu-cli/src/browse.rs next_step).
    let reason;
    if (!view.snapshot.built) {
      reason = "no tag layer in this index — run `sagasu tag` first";
    } else if (view.matched === 0) {
      reason = "nothing to add — no live file carries all of these tags";
    } else if (view.axes_refining === 0) {
      reason = "nothing to add — this group is a leaf";
    } else {
      reason = "suppressed — no value is on screen to take";
    }
    box.appendChild(el("span", "note", reason));
  }
}

function renderAxes(view) {
  const summary = byId("axes-summary");
  summary.textContent = "";
  // The user must be able to tell that more axes exist than are shown, and
  // how many of them actually refine (BrowseViewDto::axes_total/axes_refining).
  summary.appendChild(
    el(
      "span",
      null,
      `showing ${fmt(view.axes.length)} of ${fmt(view.axes_total)} axes ` +
        `present in the group; ${fmt(view.axes_refining)} of ` +
        `${fmt(view.axes_total)} could refine it`
    )
  );
  const dead = Math.max(0, view.axes_total - view.axes_refining);
  if (dead > 0) {
    summary.appendChild(
      el(
        "div",
        "note",
        `${fmt(dead)} further namespace(s) are present in the group but ` +
          `cannot narrow it — every file shares the same value, or the only ` +
          `values left are already selected`
      )
    );
  }

  const axes = byId("axes");
  axes.textContent = "";
  if (view.axes.length === 0) {
    let reason;
    if (!view.snapshot.built) {
      reason = "no tag layer in this index — run `sagasu tag` first";
    } else if (view.matched === 0) {
      reason = "no live file carries all of these tags";
    } else if (view.axes_refining === 0) {
      reason = "nothing left to drill into — this group is a leaf";
    } else {
      reason = "axes were computed but none is shown";
    }
    axes.appendChild(el("p", "note", reason));
    return;
  }
  for (const axis of view.axes) {
    axes.appendChild(renderAxis(axis));
  }
}

function renderAxis(axis) {
  const block = el("div", "axis");
  const head = el("div", "axis-head");
  head.appendChild(el("span", "axis-ns", axis.namespace));
  head.appendChild(
    el(
      "span",
      "axis-meta",
      `${axis.score.toFixed(2)} bits · ${fmt(axis.distinct)} value(s) · ` +
        `covers ${pct(axis.coverage)} of the group`
    )
  );
  block.appendChild(head);

  const values = el("div", "axis-values");
  for (const value of axis.values) {
    values.appendChild(valueChip(value));
  }
  block.appendChild(values);

  // A facet list cut short with nothing said about it reads as the whole
  // axis; the count behind the cut is the part that matters.
  if (axis.distinct > axis.values.length) {
    block.appendChild(
      el(
        "p",
        "note",
        `${fmt(axis.values.length)} of ${fmt(axis.distinct)} values shown — ` +
          `${fmt(axis.tail_assignments)} file-tag(s) behind the rest`
      )
    );
  }
  // "covers 100%" above shares adding to 232% is a multi-valued namespace,
  // not a bug — but nothing on screen says so unless it is printed.
  if (isMultiValued(axis)) {
    block.appendChild(
      el(
        "p",
        "note",
        `a file can carry several ${axis.namespace}: values, so these shares ` +
          `count some files more than once and sum past 100%`
      )
    );
  }
  return block;
}

function isMultiValued(axis) {
  const shown = axis.values.reduce((sum, v) => sum + v.files, 0);
  return shown + axis.tail_assignments > axis.files;
}

function valueChip(value) {
  const b = el("button", "chip");
  b.title = value.tag.tag;
  b.appendChild(el("span", "chip-value", value.tag.value));
  b.appendChild(
    el("span", "chip-count", `${fmt(value.files)} · ${pct(value.share)}`)
  );
  b.addEventListener("click", () => drill(value.tag.tag));
  return b;
}

function renderPreview(view) {
  const summary = byId("preview-summary");
  summary.textContent = "";
  const fetched = view.preview.length + view.dropped.length;
  summary.appendChild(
    el(
      "span",
      null,
      `${fmt(view.preview.length)} of ${fmt(view.matched)} shown — the first ` +
        `${fmt(fetched)} rows in file_id order, existence-checked`
    )
  );
  if (view.matched > fetched) {
    summary.appendChild(
      el("span", "note", ` — ${fmt(view.matched - fetched)} more match, not previewed`)
    );
  }

  const tbody = byId("preview-rows");
  tbody.textContent = "";
  for (const row of view.preview) {
    tbody.appendChild(previewRow(row));
  }
  if (view.preview.length === 0) {
    const tr = el("tr");
    const td = el("td", "note");
    td.colSpan = 9;
    td.textContent =
      view.matched === 0
        ? "(no files match this selection)"
        : "(no preview rows — the first files of the selection would appear here)";
    tr.appendChild(td);
    tbody.appendChild(tr);
  }
}

function previewRow(row) {
  const tr = el("tr");
  const cell = (cls, text, title) => {
    const td = el("td", cls, text);
    if (title) td.title = title;
    return td;
  };
  tr.appendChild(cell("num", String(row.file_id)));
  tr.appendChild(cell("path mono", row.path));
  tr.appendChild(cell(null, row.ext ?? ""));
  tr.appendChild(cell("num", fmt(row.size)));
  // The 2^53 guard: rendered as the string it arrived as, never a Number.
  tr.appendChild(
    cell(
      "num mono",
      row.mtime_ns,
      "nanoseconds since the epoch — kept as a string because Number would round it"
    )
  );
  tr.appendChild(
    cell(
      "num mono",
      row.ctime_ns,
      "nanoseconds since the epoch — kept as a string because Number would round it"
    )
  );
  // Byte columns arrive as lowercase hex and are displayed as such.
  tr.appendChild(cell("hex mono", row.magic ?? ""));
  tr.appendChild(cell("hex mono", row.blake3 ?? ""));
  tr.appendChild(cell("hex mono", row.fs_id ?? ""));
  return tr;
}

function renderDropped(view) {
  const panel = byId("dropped-panel");
  const box = byId("dropped");
  box.textContent = "";
  if (view.dropped.length === 0) {
    panel.hidden = true;
    return;
  }
  panel.hidden = false;
  const fetched = view.preview.length + view.dropped.length;
  box.appendChild(
    el(
      "p",
      null,
      `${fmt(view.dropped.length)} of the ${fmt(fetched)} previewed row(s) ` +
        `no longer exist on disk — the index is stale, and this list is the proof.`
    )
  );
  for (const row of view.dropped) {
    const line = el("p", "dropped-row");
    line.appendChild(el("span", "num mono", String(row.file_id)));
    line.appendChild(el("span", "mono", row.path));
    line.appendChild(
      el("span", "note", " — deleted since the index was built")
    );
    box.appendChild(line);
  }
}

// ---------------------------------------------------------------------------
// 3. the only way nodes are built
// ---------------------------------------------------------------------------

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

function byId(id) {
  return document.querySelector(`#${id}`);
}

// ---------------------------------------------------------------------------
// formatting helpers (mirror the CLI's vocabulary: sagasu-cli/src/browse.rs)
// ---------------------------------------------------------------------------

function pct(share) {
  const p = 100 * share;
  if (share > 0 && p < 0.5) return "<1%";
  return `${Math.round(p)}%`;
}

function fmt(n) {
  return n.toLocaleString("en-US");
}

// ---------------------------------------------------------------------------
// 4. wiring: clicks, crumbs, location.hash
// ---------------------------------------------------------------------------

let currentSelection = [];

function selectionKey(selection) {
  return JSON.stringify(selection);
}

function selectionFromHash() {
  const h = location.hash;
  if (!h || h === "#") return [];
  try {
    const v = JSON.parse(decodeURIComponent(h.slice(1)));
    if (Array.isArray(v) && v.every((t) => typeof t === "string")) return v;
  } catch {
    // fall through
  }
  return null; // malformed hash — caller decides what to do with it
}

function go(selection) {
  currentSelection = selection.slice();
  // The selection lives in the hash, so reload and the browser back button
  // both work, and a drilled state is deep-linkable.
  location.hash = `#${encodeURIComponent(selectionKey(selection))}`;
  return showSelection(currentSelection);
}

function drill(tagString) {
  if (currentSelection.includes(tagString)) return; // same group — not offered anyway
  go(currentSelection.concat(tagString));
}

async function showSelection(selection) {
  clearNotice();
  try {
    const view = await loadBrowseView(selection);
    render(view, selection);
  } catch (err) {
    if (err.noFixture) {
      showNoFixture(selection, err.known);
    } else {
      showLoadError(err);
    }
  }
}

function clearNotice() {
  byId("notice-panel").hidden = true;
  byId("notice").textContent = "";
}

function showNoFixture(selection, known) {
  const box = byId("notice");
  box.textContent = "";
  box.appendChild(el("p", "notice-title", "No fixture for this selection"));
  box.appendChild(
    el(
      "p",
      null,
      `The selection ${selection.length === 0 ? "(whole index)" : selection.join(" AND ")} ` +
        `has no fixture in this mock.`
    )
  );
  box.appendChild(
    el(
      "p",
      null,
      `Fixtures exist for these selections only: ${known.join(", ")}.`
    )
  );
  box.appendChild(
    el(
      "p",
      "note",
      "A real backend would compute this view. This is a hole in the mock, " +
        "not an empty result set."
    )
  );
  byId("notice-panel").hidden = false;
}

function showLoadError(err) {
  const box = byId("notice");
  box.textContent = "";
  box.appendChild(el("p", "notice-title", "Could not load a fixture"));
  box.appendChild(el("p", "mono", err.message));
  byId("notice-panel").hidden = false;
}

// Browser back/forward and manual hash edits re-load the state they point at.
window.addEventListener("hashchange", () => {
  const sel = selectionFromHash();
  if (sel !== null && selectionKey(sel) !== selectionKey(currentSelection)) {
    currentSelection = sel;
    showSelection(currentSelection);
  }
});

// Boot: whatever the hash says, or the exploration root.
const initial = selectionFromHash();
currentSelection = initial === null ? [] : initial;
showSelection(currentSelection);