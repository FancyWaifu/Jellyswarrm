"""SSO in Chromium through to jellyfin-web 12.2: Keycloak login, link to an existing
account, then the completion page's hand-off must land on home (twice: first link,
then a returning login)."""
import os, re, time
from playwright.sync_api import sync_playwright
from lib import *

SHOTS = "/state/shots-sso"; os.makedirs(SHOTS, exist_ok=True)
wait(f"{P}/System/Info/Public"); wait("http://keycloak:8080/realms/jsw/.well-known/openid-configuration", 300); time.sleep(30)
api_login(P, "admin", "password")  # creates the Jellyswarrm user the SSO identity links to
errors = []

def kc_login(page):
    page.goto(f"{P}/sso/login/kc")
    page.wait_for_selector("#username", timeout=60000)
    page.fill("#username", "kclinker"); page.fill("#password", "kcpass"); page.click("#kc-login")

def lands_home(page, name, n):
    try:
        page.wait_for_url(re.compile(r"#/home"), timeout=60000); page.wait_for_selector(".card", timeout=60000)
        ok, detail = True, page.url
    except Exception as e:
        ok, detail = False, f"url={page.url} {str(e)[:60]}"
    page.screenshot(path=f"{SHOTS}/{n}.png")
    check(name, ok, detail)

with sync_playwright() as pw:
    b = pw.chromium.launch()
    page = b.new_page(viewport={"width": 1280, "height": 800}, locale="en-US")
    page.on("pageerror", lambda e: errors.append(str(e)[:200]))
    kc_login(page)
    page.wait_for_selector("form[action='/sso/link'] input[name=username]", timeout=60000)
    page.fill("input[name=username]", "admin"); page.fill("input[name=password]", "password")
    page.click("form[action='/sso/link']:has(input[name=username]) button")
    lands_home(page, "first SSO login (link existing account) lands on jellyfin-web home", "01-linked")

    page.context.clear_cookies(); page.evaluate("localStorage.clear()")
    kc_login(page)
    lands_home(page, "returning SSO login lands on jellyfin-web home", "02-returning")
    b.close()
check("no uncaught JS exceptions", not errors, str(errors[:3]))
summary()
