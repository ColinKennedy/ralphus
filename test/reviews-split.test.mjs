// Coverage for RAL-417: the Reviews sidebar's draggable resizer.
//
// The Squads page has had a drag-resizable sidebar (`.splitter` over a
// var-driven grid track, RAL-12, widths persisted to localStorage) since
// long before the Reviews page existed; the Reviews page's sidebar stayed a
// fixed 300px. This pins:
//
//   * the new `--reviews-sidebar-w` grid track + `.splitter` element, wired
//     into the *shared* SPLIT_CFG/initSplitters drag lifecycle so resizing
//     the Reviews sidebar behaves exactly like resizing the Squads one;
//   * the persistence separation: the Reviews width lives under its own
//     localStorage key (`ralphus-reviews-sidebar-w`), so dragging Reviews
//     never moves the Squads sidebar and vice versa;
//   * the 240px review-detail-pane floor (`MIN_REVIEW_DETAIL_W`), enforced
//     by the dynamic `reviewsSidebarMaxW()` ceiling that `paneMax` routes
//     the reviews var through -- same guard-rail pattern as MIN_CENTER_W;
//   * the small-screen behavior: below 720px (every iPhone in portrait) the
//     two-pane grid stacks (list on top, bounded to 45vh with its own
//     scroll, detail full-width below), the splitter is hidden (mouse-drag
//     resizing is unusable on touch), and the header tabs wrap so the
//     Reviews tab is reachable at all.
//
// The max-width math is exercised as pure logic (sliced from the real chunk
// via ./board-reviews-split.mjs); the HTML/CSS/JS wiring is pinned with
// source-shape assertions over the shipped board.html / board.css / chunk
// text, which is this repo's browser coverage for a project with no browser
// harness.
//
// Run with `npm test` (node --test).

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { boardScript } from "./board-source.mjs";
import { loadReviewsSplit } from "./board-reviews-split.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const boardHtml = readFileSync(join(repoRoot, "librarian", "assets", "board.html"), "utf8");
const boardCss = readFileSync(join(repoRoot, "librarian", "assets", "board.css"), "utf8");
const boardSource = boardScript();

// ---------- pure max-width math (sliced straight out of 25-chrome.js) ----------

test("reviewsSidebarMaxWFor keeps the detail pane at exactly 240px on desktop viewports", () => {
  const api = loadReviewsSplit();
  for (const viewport of [1024, 1280, 1440, 1920]) {
    const max = api.reviewsSidebarMaxWFor(viewport);
    assert.equal(viewport - max - 6, 240, `detail-pane width at viewport ${viewport}`);
  }
});

test("reviewsSidebarMaxWFor never drops below the sidebar's own min on phone-sized viewports", () => {
  const api = loadReviewsSplit();
  // iPhone portrait widths are 320-430 CSS px; below the balance point
  // (min 180 + splitter 6 + detail floor 240 = 426) a naive "keep the detail
  // at 240" ceiling would leave nothing for the sidebar, so the floor must
  // win -- the small-screen media query stacks the panes at those viewports
  // anyway.
  for (const viewport of [320, 360, 375, 390, 425]) {
    assert.equal(api.reviewsSidebarMaxWFor(viewport), 180, `viewport ${viewport}`);
  }
});

test("the ceiling engages exactly at min-sidebar + splitter + 240px of detail", () => {
  const api = loadReviewsSplit();
  assert.equal(api.reviewsSidebarMaxWFor(425), 180, "a 425px viewport is still floor-bound");
  const at = api.reviewsSidebarMaxWFor(426);
  assert.equal(at, 180, "426px is the narrowest viewport where both panes can coexist");
  assert.equal(426 - at - 6, 240);
  const above = api.reviewsSidebarMaxWFor(427);
  assert.equal(above, 181, "past the balance point the ceiling grows one-for-one with the viewport");
  assert.equal(427 - above - 6, 240);
});

test("reviewsSidebarMaxW feeds the current window width into the pure math", () => {
  const api = loadReviewsSplit({ innerWidth: 1200 });
  assert.equal(api.reviewsSidebarMaxW(), api.reviewsSidebarMaxWFor(1200));
  assert.equal(api.reviewsSidebarMaxW(), 1200 - 6 - 240);
});

// ---------- persistence: the Reviews preference is independent of Squads' ----------

test("SPLIT_CFG carries a reviews-sidebar entry with its own localStorage key", () => {
  const reviews = boardSource.match(/"--reviews-sidebar-w": \{\s*key: "([^"]+)"\s*,\s*def: (\d+),\s*min: (\d+)\s*\}/);
  const squads = boardSource.match(/"--sidebar-w": \{\s*key: "([^"]+)"\s*,\s*def: \d+,\s*min: \d+,\s*max: \d+\s*\}/);
  assert.ok(reviews && squads, "SPLIT_CFG must carry both --sidebar-w and --reviews-sidebar-w entries");
  assert.equal(reviews[1], "ralphus-reviews-sidebar-w", "the Reviews width must persist under its own key");
  assert.notEqual(reviews[1], squads[1], "the Reviews preference must not share the Squads sidebar's key");
  assert.equal(reviews[2], "300", "the Reviews default must match the old fixed 300px grid track");
  assert.equal(reviews[3], "180", "the Reviews min must match the floor the pure logic was evaluated against");
});

test("applyPaneWidths restores every SPLIT_CFG entry from its own localStorage key", () => {
  assert.match(boardSource, /for \(const \[v, c\] of Object\.entries\(SPLIT_CFG\)\)/, "applyPaneWidths must walk every pane config -- a new entry needs no extra wiring");
  assert.match(boardSource, /localStorage\.getItem\(c\.key\)/, "each width is read back from the pane's own localStorage key");
});

// ---------- drag lifecycle wiring ----------

test("paneMax routes the reviews var through the details-aware ceiling", () => {
  assert.match(boardSource, /varName === "--reviews-sidebar-w" \? reviewsSidebarMaxW\(\)/, "paneMax must consult reviewsSidebarMaxW for the reviews var");
});

test("initSplitters registers the reviews-page splitter with the shared mousedown drag lifecycle", () => {
  assert.match(
    boardSource,
    /document\.querySelectorAll\("#squads-page \.splitter, #tasks-page \.splitter, #reviews-page \.splitter"\)/,
    "the reviews splitter must join the same mousedown/mousemove/mouseup drag handling as the squads and tasks splitters",
  );
});

// ---------- board.html wiring ----------

test("board.html puts the splitter between the Reviews sidebar and the detail pane", () => {
  const reviews = boardHtml.slice(boardHtml.indexOf('<main id="reviews-page"'));
  const centerAt = reviews.indexOf('<div class="col center">');
  const splitterAt = reviews.indexOf('class="splitter"');
  assert.ok(centerAt > -1, "the reviews page must keep its center (detail) column");
  assert.ok(splitterAt > -1 && splitterAt < centerAt, "the splitter must sit between the sidebar and the center column");
  assert.match(reviews, /class="splitter" data-var="--reviews-sidebar-w"/, "the splitter must drive the --reviews-sidebar-w track");
  assert.match(reviews.slice(0, centerAt), /class="splitter"[^>]*data-tip="[^"]+"/, "the splitter must carry a tooltip per the board's tooltip rule (RAL-40)");
});

test("the Reviews splitter carries no data-invert (dragging right widens the sidebar)", () => {
  const reviews = boardHtml.slice(boardHtml.indexOf('<main id="reviews-page"'));
  const splitterAt = reviews.indexOf('class="splitter"');
  const openTag = reviews.slice(splitterAt, reviews.indexOf(">", splitterAt) + 1);
  assert.doesNotMatch(openTag, /data-invert/, "only the right-edge detail-pane splitters invert; the reviews sidebar grows with a rightward drag");
  assert.match(openTag, /data-var="--reviews-sidebar-w"/);
});

// ---------- board.css wiring (the closest thing this repo has to browser coverage) ----------

test("board.css drives the Reviews grid off the --reviews-sidebar-w track with a 6px splitter column", () => {
  assert.match(boardCss, /main#reviews-page \{ display: grid; grid-template-columns: var\(--reviews-sidebar-w, 300px\) 6px 1fr; height: calc\(100vh - 51px\); \}/);
});

test("narrow screens stack the Reviews panes, hide the splitter, and wrap the header tabs", () => {
  const small = boardCss.slice(boardCss.indexOf("@media (max-width: 720px)"));
  assert.ok(small.length > 0, "a small-screen media query must exist for iPhones and other narrow viewports");
  assert.match(small, /header \{ flex-wrap: wrap; \}/, "the header tabs wrap so the Reviews tab is reachable");
  assert.match(small, /\.tabs \{ flex-wrap: wrap; \}/);
  assert.match(small, /main#reviews-page \{ grid-template-columns: 1fr; height: auto; overflow-y: auto; \}/, "the two panes stack and the page scrolls as one");
  assert.match(small, /main#reviews-page \.splitter \{ display: none; \}/, "the mouse-drag splitter is hidden on touch-sized screens");
  assert.match(small, /main#reviews-page \.sidebar \{ max-height: 45vh; \}/, "the review list stays bounded so the selected review's detail is reachable below it");
});