// Behavioral test for the M4 step 3 browse frontend (scaffolding, not a
// product test): loads ui/app.js in a Node vm with a faithful-enough DOM
// shim and drives the acceptance flow — root render, drill down, the
// no-fixture hole, the way back, and the string-fidelity rules (mtime_ns
// never a Number, hex columns as hex).
//
// Run: node ui/tests/browse.test.mjs
//
// The shim implements exactly the DOM surface app.js uses (createElement,
// querySelector by id, textContent, appendChild, className, classList.add,
// disabled, hidden, title, colSpan, addEventListener). Anything app.js needs
// that the shim lacks fails loudly here, which is the point.

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const UI_DIR = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const FIXTURES_DIR = path.join(UI_DIR, "fixtures");

// ---------------------------------------------------------------------------
// the DOM shim
// ---------------------------------------------------------------------------

class El {
  constructor(tag) {
    this.tag = tag;
    this.children = [];
    this.className = "";
    this.disabled = false;
    this.hidden = false;
    this.title = "";
    this.colSpan = 1;
    this._text = "";
    this._listeners = {};
    this._classes = new Set();
  }
  get textContent() {
    return this._text;
  }
  set textContent(value) {
    // A textContent write replaces all children — same as the real DOM.
    this._text = String(value);
    this.children = [];
  }
  appendChild(child) {
    this.children.push(child);
    return child;
  }
  addEventListener(type, fn) {
    (this._listeners[type] ??= []).push(fn);
  }
  fire(type) {
    for (const fn of this._listeners[type] ?? []) fn({ type });
  }
  get classList() {
    return {
      add: (name) => this._classes.add(name),
      contains: (name) => this._classes.has(name),
    };
  }
}

function fullText(node) {
  let out = node._text ?? "";
  for (const child of node.children) out += fullText(child);
  return out;
}

function walk(root, out = []) {
  out.push(root);
  for (const child of root.children) walk(child, out);
  return out;
}

// The containers app.js resolves by id — the ids in ui/index.html.
const CONTAINER_IDS = [
  "snapshot", "crumbs", "counts", "label", "universal", "recommended",
  "axes-summary", "axes", "preview-summary", "preview-rows",
  "dropped-panel", "dropped", "notice-panel", "notice",
];

const containers = new Map();
for (const id of CONTAINER_IDS) containers.set(`#${id}`, new El("div"));

const document = {
  querySelector(selector) {
    return containers.get(selector) ?? null;
  },
  createElement(tag) {
    return new El(tag);
  },
};

// location: a hash with a real setter, so app.js's go() round-trips it.
let hashValue = "";
const location = {
  get hash() {
    return hashValue;
  },
  set hash(v) {
    hashValue = v;
  },
};

// fetch: reads the committed fixtures from disk, like a static server would.
async function fetchStub(url) {
  const rel = url.replace(/^fixtures\//, "");
  const body = await fs.promises.readFile(path.join(FIXTURES_DIR, rel), "utf8");
  return { ok: true, status: 200, json: async () => JSON.parse(body) };
}

const sandbox = {
  document,
  location,
  fetch: fetchStub,
  console,
  window: null, // set below — must be the vm global itself
};
const context = vm.createContext(sandbox);
sandbox.window = context;
const hashListeners = [];
context.addEventListener = (type, fn) => {
  if (type === "hashchange") hashListeners.push(fn);
};

// Simulate the browser back/forward button: the hash changes and the
// registered hashchange listeners run, exactly as they would in a browser.
function simulateHash(newHash) {
  hashValue = newHash;
  for (const fn of hashListeners) fn();
}

const appSource = fs.readFileSync(path.join(UI_DIR, "app.js"), "utf8");
vm.runInContext(appSource, context, { filename: "app.js" });

const tick = () => new Promise((r) => setTimeout(r, 20));
const byId = (id) => containers.get(`#${id}`);
const text = (id) => fullText(byId(id));
const allNodes = () =>
  [...containers.values()].flatMap((root) => walk(root));
const buttons = () => allNodes().filter((n) => n.tag === "button");
const chipWithValue = (value) =>
  buttons().find(
    (b) =>
      b.className === "chip" &&
      b.children[0]?.tag === "span" &&
      b.children[0]._text === value
  );
const recommendedCard = () =>
  buttons().find((b) => b.className === "recommended-card");
const crumbWithText = (t) =>
  buttons().find((b) => b.className === "crumb" && fullText(b) === t);

const rootFixture = JSON.parse(
  fs.readFileSync(path.join(FIXTURES_DIR, "browse-root.json"), "utf8")
);
const drilledFixture = JSON.parse(
  fs.readFileSync(path.join(FIXTURES_DIR, "browse-drilled.json"), "utf8")
);

let failures = 0;
function check(name, cond, detail) {
  if (cond) {
    console.log(`ok   - ${name}`);
  } else {
    failures += 1;
    console.log(`FAIL - ${name}${detail ? `\n       ${detail}` : ""}`);
  }
}

// ---------------------------------------------------------------------------
// acceptance flow
// ---------------------------------------------------------------------------

await tick(); // let the boot render finish

// --- root state ---
check("root: counts render matched vs corpus", text("counts").includes("108 of 108 live files"), text("counts"));
check(
  "root: matched is presented as an upper bound",
  text("counts").includes("upper bound"),
  text("counts")
);
check(
  "root: label terms render with counts",
  text("label").includes("format:text") && text("label").includes("(107/108)"),
  text("label")
);
check(
  "root: label vocabulary trust note",
  text("label").includes("c-TF-IDF over 52 candidate tags"),
  text("label")
);
check(
  "root: snapshot says the tag layer is a snapshot",
  text("snapshot").includes("Tag layer is a snapshot of crawl 1"),
  text("snapshot")
);
check(
  "root: recommended is a card of its own, not an axis row",
  recommendedCard() !== undefined &&
    fullText(recommendedCard()).includes("ext:rs") &&
    fullText(recommendedCard()).includes("bits"),
  text("recommended")
);
check(
  "root: axes_total/axes_refining visible",
  text("axes-summary").includes("showing 4 of 6 axes") &&
    text("axes-summary").includes("6 of 6 could refine"),
  text("axes-summary")
);
check(
  "root: kind:code is a clickable axis value",
  chipWithValue("code") !== undefined
);
check(
  "root: dropped panel is hidden when empty",
  byId("dropped-panel").hidden === true
);
check("root: no notice at boot", byId("notice-panel").hidden === true);

// mtime_ns must be rendered verbatim — as the decimal string it arrived as.
const rootMtime = rootFixture.preview[0].mtime_ns;
check(
  "root: mtime_ns rendered verbatim (never a Number)",
  text("preview-rows").includes(rootMtime) && /^\d{19}$/.test(rootMtime),
  `${rootMtime} vs ${text("preview-rows")}`
);
check(
  "root: hex columns rendered as-is",
  text("preview-rows").includes(rootFixture.preview[0].magic ?? "(none)") ||
    rootFixture.preview[0].magic === null,
  text("preview-rows")
);

// --- drill down: click kind:code ---
chipWithValue("code").fire("click");
await tick();

check(
  "drilled: selection crumb shows the tag",
  text("crumbs").includes("kind:code"),
  text("crumbs")
);
check(
  "drilled: counts narrowed",
  text("counts").includes("66 of 108 live files"),
  text("counts")
);
check(
  "drilled: label changed",
  text("label").includes("ext:rs") && text("label").includes("(60/66)"),
  text("label")
);
check(
  "drilled: universal renders as a fact, not a step",
  text("universal").includes("format:text") &&
    text("universal").includes("Choosing one returns the same group"),
  text("universal")
);
check(
  "drilled: axes summary says 2 of 4 axes, 2 refine",
  text("axes-summary").includes("showing 2 of 4 axes") &&
    text("axes-summary").includes("2 of 4 could refine"),
  text("axes-summary")
);
check(
  "drilled: multi-valued path note",
  text("axes").includes("sum past 100%"),
  text("axes")
);
check(
  "drilled: recommended follows the new selection",
  fullText(recommendedCard() ?? { children: [] }).includes("path:core"),
  text("recommended")
);
check(
  "drilled: dropped is on screen, not in a console",
  byId("dropped-panel").hidden === false &&
    text("dropped").includes("no longer exist on disk"),
  text("dropped")
);
check(
  "drilled: the dropped row names the deleted path",
  text("dropped").includes(drilledFixture.dropped[0].path) &&
    text("dropped").includes("deleted since the index was built"),
  text("dropped")
);
check(
  "drilled: dropped row is not in the preview table",
  !text("preview-rows").includes(drilledFixture.dropped[0].path),
  text("preview-rows")
);
check(
  "drilled: preview rows are the live ones only",
  text("preview-rows").includes(drilledFixture.preview[0].path),
  text("preview-rows")
);
const drilledMtime = drilledFixture.preview[0].mtime_ns;
check(
  "drilled: mtime_ns still verbatim",
  text("preview-rows").includes(drilledMtime),
  `${drilledMtime} vs ${text("preview-rows")}`
);

// --- a selection with no fixture says so ---
chipWithValue("src").fire("click");
await tick();

check(
  "no-fixture: notice is visible",
  byId("notice-panel").hidden === false,
  text("notice")
);
check(
  "no-fixture: names the selection",
  text("notice").includes("kind:code AND path:src"),
  text("notice")
);
check(
  "no-fixture: distinct from an empty result set",
  text("notice").includes("not an empty result set"),
  text("notice")
);

// --- the way back, via the hash (browser back button) ---
simulateHash(`#${encodeURIComponent(JSON.stringify(["kind:code"]))}`);
await tick();

check(
  "back: drilled state again",
  text("counts").includes("66 of 108 live files") && byId("notice-panel").hidden === true,
  `${text("counts")} | notice hidden=${byId("notice-panel").hidden}`
);
check(
  "back: dropped still on screen",
  byId("dropped-panel").hidden === false,
  text("dropped")
);

// --- the way back, via the crumb trail ---
crumbWithText("whole index").fire("click");
await tick();

check(
  "crumb: back to the root",
  text("counts").includes("108 of 108 live files") && byId("dropped-panel").hidden === true,
  `${text("counts")} | dropped hidden=${byId("dropped-panel").hidden}`
);

// --- the recommended card is itself a drill. At the root the recommendation
// is ext:rs, which has no fixture — so the correct behavior is the hole
// being named on screen, not a silently empty view. ---
recommendedCard().fire("click");
await tick();
check(
  "recommended card drills into the hole it names",
  byId("notice-panel").hidden === false &&
    text("notice").includes("ext:rs") &&
    text("notice").includes("not an empty result set"),
  text("notice")
);

// ---------------------------------------------------------------------------

if (failures > 0) {
  console.error(`\n${failures} check(s) failed`);
  process.exit(1);
}
console.log("\nall checks passed");