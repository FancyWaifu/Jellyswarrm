"""jellyfin-web on the big merged library: page through, Genres tab, filter menu."""
import os, re, time
from playwright.sync_api import sync_playwright
from lib import *

SHOTS = "/state/shots"; os.makedirs(SHOTS, exist_ok=True)
for host in ("http://jf-a:8096", "http://jf-b:8096"):  # wait for the full scan
    h = jf_admin(host)
    while req("GET", f"{host}/Items?Recursive=true&IncludeItemTypes=Movie&Limit=0", headers=h)[1]["TotalRecordCount"] < 130:
        time.sleep(2)
wait(f"{P}/System/Info/Public"); time.sleep(40)
st, a = api_login(P, "admin", "password"); uid = a["User"]["Id"]; H = hdr(token=a["AccessToken"])
st, views = req("GET", f"{P}/Users/{uid}/Views", headers=H)
lib = next(v["Id"] for v in views["Items"] if v.get("CollectionType") == "movies")
errors, failed = [], []
with sync_playwright() as pw:
    page = pw.chromium.launch().new_page(viewport={"width": 1280, "height": 900}, locale="en-US")
    page.on("pageerror", lambda e: errors.append(str(e)[:200]))
    page.on("response", lambda r: r.status >= 500 and failed.append((r.status, r.url[:120])))
    page.goto(f"{P}/", wait_until="domcontentloaded")
    page.wait_for_selector("#loginPage", timeout=60000)
    if page.locator(".manualLoginForm.hide").count():
        page.click("button:has-text('Manual Login'), .btnManual", timeout=10000)
    page.fill("#txtManualName", "admin"); page.fill("#txtManualPassword", "password")
    page.click(".manualLoginForm button[type=submit]")
    page.wait_for_url(re.compile(r"#/home"), timeout=60000)

    page.goto(f"{P}/#/movies?topParentId={lib}&collectionType=movies")
    page.wait_for_selector(".card", timeout=60000); time.sleep(3)
    def view():
        chip = page.locator("text=/^\\d+-\\d+ of \\d+$/").first.inner_text()
        names = [t.split(" [")[0] for t in page.locator(".card .cardText").all_inner_texts() if t.startswith("Film")]
        return chip, names
    def next_page():
        page.locator("button[title='Next']").first.click(); time.sleep(4)
    p1, names1 = view()
    page.screenshot(path=f"{SHOTS}/big-01-page1.png")
    check("page 1 shows Film 000-099", len(names1) == 100 and names1[0] == "Film 000" and names1[-1] == "Film 099", f"chip='{p1}' n={len(names1)} first={names1[:1]} last={names1[-1:]}")
    next_page(); p2, names2 = view()
    check("next page shows Film 100-199", len(names2) == 100 and names2[0] == "Film 100" and names2[-1] == "Film 199", f"chip='{p2}' n={len(names2)} first={names2[:1]} last={names2[-1:]}")
    next_page(); p3, names3 = view()
    page.screenshot(path=f"{SHOTS}/big-02-page3.png")
    check("last page shows Film 200-229, chip says of 230", names3[:1] == ["Film 200"] and names3[-1:] == ["Film 229"] and p3.endswith("of 230"), f"chip='{p3}' n={len(names3)}")

    page.locator("button[title='Filter']").first.click(); time.sleep(2)
    page.locator("[aria-controls='filtersGenres-content']").first.click(); time.sleep(2)
    text = page.locator("#filtersGenres-content").inner_text()
    page.screenshot(path=f"{SHOTS}/big-03-filters.png")
    check("filter menu offers genres from both servers", all(g in text for g in ("Comedy", "Drama", "Horror")), text.replace("\n", " | ")[:200])
    page.locator("#filtersGenres-content label:has-text('Horror')").first.click(); time.sleep(4)
    page.keyboard.press("Escape"); time.sleep(3)
    films = [t for t in page.locator(".card .cardText").all_inner_texts() if t.startswith("Film")]
    page.screenshot(path=f"{SHOTS}/big-04-horror.png")
    check("picking Horror (only on B) shows B's 65 films", len(films) == 65 and all("[B]" in t for t in films), f"n={len(films)} first={films[:2]}")

    page.goto(f"{P}/#/movies?topParentId={lib}&collectionType=movies"); time.sleep(3)
    page.locator("button:has-text('Movies')").first.click(); time.sleep(1)
    page.locator("[role=menuitem]:has-text('Genres')").first.click()
    try:
        page.wait_for_selector("text=Horror", timeout=30000)
    except Exception:
        pass
    time.sleep(2)
    gtext = page.locator("body").inner_text()
    page.screenshot(path=f"{SHOTS}/big-05-genres.png")
    check("Genres view shows Comedy, Drama and Horror", all(g in gtext for g in ("Comedy", "Drama", "Horror")), gtext.replace("\n", " | ")[:200])
check("no uncaught JS exceptions", not errors, str(errors[:3]))
check("no 5xx responses", not failed, str(failed[:3]))
summary()
