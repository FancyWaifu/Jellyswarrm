"""Real jellyfin-web 12.2 in headless Chromium against the proxy (mixed backends)."""
import json, os, re, time
from playwright.sync_api import sync_playwright
from lib import *

SHOTS = "/state/shots"; os.makedirs(SHOTS, exist_ok=True)
wait(f"{P}/System/Info/Public"); time.sleep(40)
server_id = req("GET", f"{P}/System/Info/Public")[1]["Id"]
st, a = api_login(P, "admin", "password"); tok, uid = a["AccessToken"], a["User"]["Id"]; H = hdr(token=tok)
st, views = req("GET", f"{P}/Users/{uid}/Views", headers=H)
lib = next(v["Id"] for v in views["Items"] if v.get("CollectionType") == "movies")
st, items = req("GET", f"{P}/Items?ParentId={lib}&Recursive=true&IncludeItemTypes=Movie&userId={uid}", headers=H)
movies = {f"{i['Name']} {i.get('ProductionYear')}": i["Id"] for i in items["Items"]}
print("movies:", list(movies))

errors, failed = [], []
with sync_playwright() as pw:
    browser = pw.chromium.launch(args=["--autoplay-policy=no-user-gesture-required"])
    # Real browsers report a BCP 47 tag; the container default "en-US@posix" crashes
    # jellyfin-web 12.2 itself (Number.toLocaleString RangeError), proxy or not.
    page = browser.new_page(viewport={"width": 1280, "height": 800}, locale="en-US")
    # Offline network: the Chromecast SDK (gstatic) would stall each page load on DNS.
    page.route(re.compile(r"^https?://(?!proxy[:/])"), lambda r: r.abort())
    page.on("console", lambda m: m.type == "error" and errors.append((page.url.split("#")[-1][:40], m.text[:200])))
    page.on("pageerror", lambda e: errors.append((page.url.split("#")[-1][:40], "UNCAUGHT " + str(e)[:200])))
    page.on("response", lambda r: r.status >= 500 and failed.append((r.status, r.url[:120])))

    page.goto(f"{P}/", wait_until="domcontentloaded")
    page.wait_for_selector("#loginPage", timeout=60000)
    if page.locator(".manualLoginForm.hide").count():
        page.click("button:has-text('Manual Login'), .btnManual", timeout=10000)
    page.fill("#txtManualName", "admin"); page.fill("#txtManualPassword", "password")
    page.click(".manualLoginForm button[type=submit]")
    page.wait_for_url(re.compile(r"#/home"), timeout=60000)
    page.wait_for_selector(".card", timeout=60000)
    page.screenshot(path=f"{SHOTS}/01-home.png")
    check("login + home page renders cards", page.locator(".card").count() > 0, f"cards={page.locator('.card').count()}")
    def user_menu():  # jellyfin-web 12's default (modern) layout: the switcher lives in the user menu
        page.click("button[aria-controls=app-user-menu]")
        page.wait_for_selector("#app-user-menu [role=menuitem].js-server-item", state="visible", timeout=30000)
        return [t.strip("▸ \u00a0") for t in page.locator("#app-user-menu [role=menuitem].js-server-item").all_inner_texts()]
    try:  # injected after login without a reload: /servers now needs the session token
        nav = user_menu()
    except Exception as e:
        nav = [f"missing: {str(e)[:80]}"]
    check("server switcher is visible in the user menu and lists both backends", {"A", "B", "All servers"} <= set(nav), str(nav))
    try:
        with page.expect_navigation(timeout=60000):
            page.locator("#app-user-menu [role=menuitem].js-server-item", has_text="B").first.click()
        page.wait_for_url(re.compile(r"#/home"), timeout=60000); page.wait_for_selector(".card", timeout=60000)
        page.wait_for_load_state("networkidle"); time.sleep(2)
        libs = page.locator(".card").all_inner_texts()
        check("picking a server scopes the home page to it", any("[B]" in t for t in libs) and not any("[A]" in t for t in libs), str(libs[:6]))
        check("the picked server is marked in the menu", "▸ B" in " | ".join(page.evaluate("[...document.querySelectorAll('#app-user-menu .js-server-item')].map(e=>e.textContent)")))
        user_menu()
        with page.expect_navigation(timeout=60000):
            page.locator("#app-user-menu [role=menuitem].js-server-item", has_text="All servers").click()
        page.wait_for_url(re.compile(r"#/home"), timeout=60000); page.wait_for_selector(".card", timeout=60000)
    except Exception as e:
        check("picking a server scopes the home page to it", False, str(e)[:150])

    page.goto(f"{P}/#/movies?topParentId={lib}&collectionType=movies")
    try:
        page.wait_for_selector("text=Halloween", timeout=60000)
    except Exception:
        pass
    time.sleep(3)
    page.screenshot(path=f"{SHOTS}/02-library.png")
    text = page.locator("body").inner_text()
    check("movie library page has no error boundary", "Error" not in text.split("\n")[0:40].__str__() or "RangeError" not in text, text[:120].replace("\n", " "))
    check("movie library lists items from both backends", "[A]" in text and "[B]" in text and text.count("Halloween") >= 2,
          f"found={[l for l in text.splitlines() if 'Halloween' in l or 'Synced' in l][:6]}")

    for n, (name, item_id) in enumerate(movies.items(), 3):
        page.goto(f"{P}/#/details?id={item_id}&serverId={server_id}")
        page.wait_for_selector("#itemDetailPage:visible h1.itemName", timeout=60000); time.sleep(3)
        page.screenshot(path=f"{SHOTS}/{n:02d}-details-{name.replace(' ', '_')}.png")
        body = page.locator("#itemDetailPage:visible").inner_text()
        check(f"details page renders: {name}", name.split(" [")[0] in body, body[:80].replace("\n", " "))
        play = page.locator("#itemDetailPage:visible button[data-action=play]:visible, #itemDetailPage:visible button[data-action=resume]:visible").first
        play.click(timeout=15000)
        try:
            page.wait_for_selector("video", timeout=60000)
            page.wait_for_function("() => { const v = document.querySelector('video'); return v && v.currentTime > 2; }", timeout=90000)
            t = page.evaluate("document.querySelector('video').currentTime")
            src = page.evaluate("document.querySelector('video').currentSrc || ''")
            check(f"playback starts: {name}", True, f"currentTime={t:.1f}s src={src[:70]}")
        except Exception as e:
            check(f"playback starts: {name}", False, str(e)[:150])
        page.screenshot(path=f"{SHOTS}/{n:02d}-playing-{name.replace(' ', '_')}.png")
        page.goto(f"{P}/#/home"); time.sleep(2)
    browser.close()

json.dump({"errors": errors, "failed": failed}, open("/state/ui_errors.json", "w"), indent=1)
print(f"\nconsole errors ({len(errors)}):"); [print("  ", e) for e in errors]
print(f"proxy 5xx ({len(failed)}):"); [print("  ", f) for f in failed]
check("no uncaught JS exceptions", not any(e[1].startswith("UNCAUGHT") for e in errors), f"{sum(e[1].startswith('UNCAUGHT') for e in errors)} uncaught")
check("no 5xx responses", not failed, f"{len(failed)}")
summary()
