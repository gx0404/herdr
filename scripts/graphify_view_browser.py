#!/usr/bin/env python3
"""Explicit browser acceptance for an exported local graph view (not a unit test).

Run with --html and a new --output directory. Uses the already installed Playwright
and Chromium; never installs a browser or rebuilds graph data. Evidence stays in
the repository. Screenshots still require human/visual-agent inspection.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import tempfile
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--html", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--enforce-performance", action="store_true")
    parser.add_argument("--browser", type=Path, help="Explicit installed browser executable")
    args = parser.parse_args()
    html = args.html.resolve()
    output = args.output.resolve()
    if not html.is_file():
        parser.error("--html must identify an existing exported page")
    if not output.is_relative_to(ROOT) or output == ROOT:
        parser.error("--output must be a new directory inside this repository")
    if output.exists():
        parser.error("--output must not already exist (evidence is never overwritten)")
    output.mkdir(parents=True)
    temp = output / "tmp"
    temp.mkdir()
    for key in ("TMP", "TEMP", "TMPDIR"):
        os.environ[key] = str(temp)
    tempfile.tempdir = str(temp)
    # Deliberately import after sandbox temp setup; missing runtime is a failure.
    from playwright.sync_api import sync_playwright

    report: dict = {"html": str(html), "checks": [], "screenshots": [], "errors": [], "network": []}

    def check(name: str, condition: bool) -> None:
        report["checks"].append({"name": name, "pass": bool(condition)})
        if not condition:
            raise AssertionError(name)

    def snapshot(page, name: str) -> None:
        file = output / f"{name}.png"
        page.screenshot(path=str(file), full_page=False)
        report["screenshots"].append(str(file))

    try:
        with sync_playwright() as p:
            launch = {"headless": True, "viewport": {"width": 1440, "height": 900}}
            if args.browser:
                launch["executable_path"] = str(args.browser.resolve())
            context = p.chromium.launch_persistent_context(str(output / "profile"), **launch)
            try:
                context.route("http://**/*", lambda route: (report["network"].append(route.request.url), route.abort()))
                context.route("https://**/*", lambda route: (report["network"].append(route.request.url), route.abort()))
                page = context.pages[0]
                page.on("pageerror", lambda error: report["errors"].append(str(error)))
                page.on("console", lambda message: report["errors"].append("console: " + message.text) if message.type == "error" else None)
                page.goto(html.as_uri(), wait_until="load")
                page.wait_for_selector('body[data-ready="true"]', timeout=30000)
                page.wait_for_function("window.HERDR_ATLAS_METRICS.frames > 0")
                report["initial_metrics"] = page.evaluate("window.HERDR_ATLAS_METRICS")
                counts = page.evaluate("window.HERDR_ATLAS.counts()")
                check("all input nodes and edges indexed", page.evaluate("({nodes:GRAPHIFY_VIEW_DATA.nodes.length,edges:GRAPHIFY_VIEW_DATA.links.length})") == counts)
                check("canvas displayed", page.locator("#scene canvas").is_visible())
                snapshot(page, "desktop-overview")
                first_group = page.locator("#results [data-group-key]").first
                first_group.click()
                check("group traversal", page.locator("#results [data-node-id]").count() > 0)
                snapshot(page, "desktop-group")
                page.locator("#home").click()
                samples = page.evaluate("""() => {
                    const ns=GRAPHIFY_VIEW_DATA.nodes, es=GRAPHIFY_VIEW_DATA.links;
                    const located=ns.find(n=>n.source_url && n.line);
                    const unlocated=ns.find(n=>!n.source_url);
                    const degrees=new Map();for(const e of es){degrees.set(e.source,(degrees.get(e.source)||0)+1);if(e.target!==e.source)degrees.set(e.target,(degrees.get(e.target)||0)+1);}
                    const highDegree=ns.find(n=>(degrees.get(n.id)||0)>40);
                    return {located,unlocated,highDegree,last:ns[ns.length-1],nodeCount:ns.length,edgeCount:es.length};
                }""")

                def search_id(node: dict) -> None:
                    page.locator("#search").fill(node["id"])
                    page.wait_for_function("id => window.HERDR_ATLAS.searchResultIds().includes(id)", arg=node["id"])
                    while not page.locator("#results [data-node-id]").evaluate_all("(elements,id)=>elements.some(e=>e.dataset.nodeId===id)", node["id"]):
                        check("exact search target remains reachable", page.locator("#next").is_enabled())
                        page.locator("#next").click()
                    page.locator("#results [data-node-id]").evaluate_all("(elements,id)=>elements.find(e=>e.dataset.nodeId===id).click()", node["id"])
                    check("selected exact node " + node["id"], page.evaluate("window.HERDR_ATLAS.selected") == node["id"])

                search_id(samples["located"])
                check("reliable source link", page.locator("#source-link").is_visible())
                check("keyboard focus enters details", page.locator("#detail-title").evaluate("e=>e===document.activeElement"))
                snapshot(page, "desktop-detail")
                with context.expect_page() as popup:
                    page.locator("#source-link").click()
                source = popup.value
                source.wait_for_load_state("load")
                check("source line anchor", source.locator("#L" + str(samples["located"]["line"])).count() == 1)
                snapshot(source, "source-line")
                source.close()
                page.keyboard.press("Escape")
                check("escape closes details", page.locator("#detail").is_hidden())
                check("escape returns keyboard focus", page.locator("#search").evaluate("e=>e===document.activeElement"))
                search_id(samples["unlocated"])
                check("unlocated nodes honest", page.locator("#no-source").is_visible() and page.locator("#source-link").is_hidden())
                page.keyboard.press("Escape")
                search_id(samples["last"])
                page.keyboard.press("Escape")
                if samples["highDegree"]:
                    search_id(samples["highDegree"])
                    first_relation = page.locator("#relations .relation").first.inner_text()
                    page.locator("#relation-next").click()
                    check("full relation pagination", page.locator("#relations .relation").first.inner_text() != first_relation)
                    page.locator("#relation-previous").click()
                    check("relation previous page", page.locator("#relations .relation").first.inner_text() == first_relation)
                    page.keyboard.press("Escape")
                page.locator("#search").fill("zz_no_such_node_759238492")
                page.wait_for_selector("#results .empty-state")
                snapshot(page, "empty-results")
                page.locator("#home").click()
                page.keyboard.press("/")
                check("keyboard search shortcut", page.locator("#search").evaluate("e=>e===document.activeElement"))
                page.locator("#grouping").select_option("community")
                check("all communities reachable", page.locator("#scope option").count() == page.evaluate("new Set(GRAPHIFY_VIEW_DATA.nodes.map(n=>String(n.community??'unknown'))).size + 1"))
                if page.locator("#next").is_enabled():
                    old = page.locator("#results [data-group-key]").first.get_attribute("data-group-key")
                    page.locator("#next").click()
                    check("community pagination", old != page.locator("#results [data-group-key]").first.get_attribute("data-group-key"))
                page.locator("#grouping").select_option("module")
                for width, height in ((1024, 768), (390, 844)):
                    page.set_viewport_size({"width": width, "height": height})
                    page.wait_for_function("document.querySelector('#scene canvas').getBoundingClientRect().width === innerWidth")
                    page.evaluate("() => new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))")
                    page.locator("#reset-camera").click()
                    page.evaluate("() => new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))")
                    snapshot(page, f"overview-{width}")
                    check(f"no page overflow {width}", page.evaluate("document.documentElement.scrollWidth <= innerWidth"))
                    page.locator("#results [data-group-key]").first.click()
                    page.locator("#results [data-node-id]").first.click()
                    snapshot(page, f"detail-{width}")
                    check(f"close button visible {width}", page.locator("#close-detail").is_visible())
                    page.locator("#close-detail").click()
                    page.locator(".brand").click()
                page.set_viewport_size({"width": 1440, "height": 900})
                report["search_metrics"] = page.evaluate("window.HERDR_ATLAS_METRICS.filters")
                report["primary_render_timing"] = page.evaluate("""async () => {
                    const start=performance.now(), before=HERDR_ATLAS_METRICS.frames;
                    for(let i=0;i<120;i++) {
                        await new Promise(resolve=>requestAnimationFrame(resolve));
                        document.getElementById(i%2?'zoom-out':'zoom-in').click();
                    }
                    await new Promise(resolve=>requestAnimationFrame(resolve));
                    const elapsed=performance.now()-start;
                    return {mode:HERDR_ATLAS_METRICS.mode,renderer:HERDR_ATLAS_METRICS.renderer,
                        elapsedMs:elapsed,drawnFrames:HERDR_ATLAS_METRICS.frames-before,
                        drawnFps:(HERDR_ATLAS_METRICS.frames-before)*1000/elapsed,
                        heapBytes:performance.memory?performance.memory.usedJSHeapSize:null};
                }""")
                if page.evaluate("window.HERDR_ATLAS_METRICS.mode") == "3D":
                    lost = page.evaluate("""() => {
                        const gl=document.querySelector('#scene canvas').getContext('webgl2');
                        const ext=gl && gl.getExtension('WEBGL_lose_context');
                        if(ext)ext.loseContext();return Boolean(ext);
                    }""")
                    check("context loss extension available", lost)
                    page.wait_for_function("window.HERDR_ATLAS_METRICS.mode === '2D'")
                    snapshot(page, "context-loss-fallback")
                    check("context loss preserves full index", page.evaluate("window.HERDR_ATLAS.counts()") == counts)
                page.emulate_media(reduced_motion="reduce")
                page.goto(html.as_uri() + "?flat=1#node=" + quote(samples["located"]["id"], safe=""), wait_until="load")
                page.wait_for_selector('body[data-ready="true"]')
                check("2D fallback", page.evaluate("window.HERDR_ATLAS_METRICS.mode") == "2D")
                check("deep link exact node", page.evaluate("window.HERDR_ATLAS.selected") == samples["located"]["id"])
                snapshot(page, "flat-reduced-motion")
                page.locator("#close-detail").click()
                page.locator(".brand").click()
                # Measure user-triggered draws, not an artificial endless animation loop.
                timing = page.evaluate("""async () => {
                    const times=[];
                    for(let i=0;i<35;i++) {
                        const t=performance.now();document.getElementById(i%2?'zoom-out':'zoom-in').click();
                        await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
                        times.push(performance.now()-t);
                    }
                    return {interactionFrameMs:times,medianFrameMs:times.slice().sort((a,b)=>a-b)[Math.floor(times.length/2)]};
                }""")
                report["interaction_timing"] = timing
                report["final_metrics"] = page.evaluate("window.HERDR_ATLAS_METRICS")
                foreground = context.new_page()
                foreground.bring_to_front()
                hidden = page.evaluate("document.hidden")
                report["hidden_pause"] = {"visibility_hidden": hidden, "status": "PENDING"}
                if hidden:
                    before_hidden = page.evaluate("window.HERDR_ATLAS_METRICS.frames")
                    page.evaluate("document.getElementById('zoom-in').click()")
                    foreground.wait_for_timeout(150)
                    check("hidden page does not render", page.evaluate("window.HERDR_ATLAS_METRICS.frames") == before_hidden)
                    report["hidden_pause"]["status"] = "PASS"
                else:
                    report["hidden_pause"]["reason"] = "This headless browser keeps both pages visible; real background visibility not exercised."
                foreground.close()
                page.bring_to_front()
                check("offline requests absent", not report["network"])
                check("console and page errors absent", not report["errors"])
                if args.enforce_performance:
                    check("initial full index ready within 2 seconds", report["initial_metrics"]["readyMs"] <= 2000)
                    check("first visible frame within 2 seconds", report["initial_metrics"]["firstFrameMs"] <= 2000)
                    filters = report.get("search_metrics", [])
                    # Search measurements captured before the navigation below reset metrics.
                    if filters:
                        check("filter within 200 ms", max(filters) <= 200)
                    check("two rendered frames within 67 ms", timing["medianFrameMs"] <= 67)
                    check("primary renderer at least 30 drawn fps", report["primary_render_timing"]["drawnFps"] >= 30)
            finally:
                context.close()
    except Exception as exc:
        report["failure"] = f"{type(exc).__name__}: {exc}"
    finally:
        report["status"] = "FAIL" if report.get("failure") else "PASS"
        (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"status": report["status"], "report": str(output / "report.json"), "failure": report.get("failure")}, ensure_ascii=False))
    return 1 if report.get("failure") else 0


if __name__ == "__main__":
    raise SystemExit(main())
