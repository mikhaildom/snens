#!/home/clawdich/projects/snens/oneoff/gmail/.venv/bin/python
"""Gmail CLI для разбора ящика: индекс в SQLite, пачки правок, фильтры, отписки.

Строка 1-2 уезжают в описание, полный хелп — `--help`.
"""
import argparse
import base64
import json
import os
import re
import sqlite3
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

SCOPES = [
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/gmail.settings.basic",
    "https://www.googleapis.com/auth/gmail.labels",
]
CFG_DIR = Path(os.environ.get("SNENS_GMAIL_DIR", Path.home() / ".config/snens"))
PENDING = CFG_DIR / "gmail-pending.json"  # код обменивается в другом процессе: verifier нужен на диске
SECRET = Path(os.environ.get("SNENS_GMAIL_SECRET", Path.home() / "client_secret.json"))
TOKEN = CFG_DIR / "gmail-token.json"
DB = Path(os.environ.get("SNENS_GMAIL_DB", Path.home() / ".local/share/snens/mail-index.db"))


def die(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)


# ---------- авторизация ----------

def _flow():
    from google_auth_oauthlib.flow import InstalledAppFlow
    return InstalledAppFlow.from_client_secrets_file(str(SECRET), SCOPES)


def cmd_auth(a):
    if a.url:
        flow = _flow()
        flow.redirect_uri = "http://localhost:8123/"
        url, state = flow.authorization_url(access_type="offline", prompt="consent")
        CFG_DIR.mkdir(parents=True, exist_ok=True)
        PENDING.write_text(json.dumps({"state": state, "verifier": flow.code_verifier}))
        PENDING.chmod(0o600)
        print(url)
        return
    if not a.code:
        die("нужен --url или --code")
    from google.oauth2.credentials import Credentials
    raw = a.code.strip()
    if "code=" in raw:
        q = urllib.parse.parse_qs(urllib.parse.urlparse(raw).query)
        raw = q.get("code", [""])[0]
    if not raw:
        die("код не найден в строке")
    if not PENDING.exists():
        die("нет сохранённого verifier: сначала `auth --url`")
    pend = json.loads(PENDING.read_text())
    flow = _flow()
    flow.redirect_uri = "http://localhost:8123/"
    flow.code_verifier = pend["verifier"]
    flow.fetch_token(code=raw)
    PENDING.unlink(missing_ok=True)
    creds = flow.credentials
    CFG_DIR.mkdir(parents=True, exist_ok=True)
    TOKEN.write_text(creds.to_json())
    TOKEN.chmod(0o600)
    svc = build()
    p = svc.users().getProfile(userId="me").execute()
    print(f"ok: {p['emailAddress']}, всего писем {p['messagesTotal']}, тредов {p['threadsTotal']}")


def build():
    from google.oauth2.credentials import Credentials
    from google.auth.transport.requests import Request
    from googleapiclient.discovery import build as gbuild
    if not TOKEN.exists():
        die("нет токена: сначала `gmail.py auth --url` / `auth --code <code>`")
    creds = Credentials.from_authorized_user_file(str(TOKEN), SCOPES)
    if creds.expired and creds.refresh_token:
        creds.refresh(Request())
        TOKEN.write_text(creds.to_json())
        TOKEN.chmod(0o600)
    return gbuild("gmail", "v1", credentials=creds, cache_discovery=False)


# ---------- индекс ----------

def db():
    DB.parent.mkdir(parents=True, exist_ok=True)
    c = sqlite3.connect(DB)
    c.executescript("""
    create table if not exists msg(
      id text primary key, thread text, ts integer, size integer,
      from_addr text, from_name text, domain text, subject text,
      labels text, unread integer, list_unsub text, list_unsub_post text,
      unsub_done integer default 0, snippet text);
    create index if not exists msg_domain on msg(domain);
    create index if not exists msg_ts on msg(ts);
    """)
    return c


def hdr(h, name):
    for x in h.get("headers", []):
        if x["name"].lower() == name.lower():
            return x["value"]
    return ""


def parse_from(v):
    m = re.match(r'^\s*"?([^"<]*)"?\s*<([^>]+)>', v)
    if m:
        name, addr = m.group(1).strip(), m.group(2).strip()
    else:
        name, addr = "", v.strip()
    dom = addr.rsplit("@", 1)[-1].lower() if "@" in addr else addr.lower()
    return addr.lower(), name, dom


def store(c, m):
    pl = m.get("payload", {})
    h = pl.get("headers", [])
    labels = m.get("labelIds", [])
    addr, name, dom = parse_from(hdr({"headers": h}, "From"))
    c.execute("""insert or replace into msg
      (id,thread,ts,size,from_addr,from_name,domain,subject,labels,unread,
       list_unsub,list_unsub_post,snippet,unsub_done)
      values(?,?,?,?,?,?,?,?,?,?,?,?,?,
        coalesce((select unsub_done from msg where id=?),0))""", (
        m["id"], m.get("threadId"), int(m.get("internalDate", 0)) // 1000,
        int(m.get("sizeEstimate", 0)), addr, name, dom,
        hdr({"headers": h}, "Subject"),
        ",".join(labels), 0 if "UNREAD" in labels else 1,
        hdr({"headers": h}, "List-Unsubscribe"),
        hdr({"headers": h}, "List-Unsubscribe-Post"),
        m.get("snippet", ""), m["id"]))


def cmd_index(a):
    svc = build()
    c = db()
    ids = []
    page = None
    while True:
        r = svc.users().messages().list(
            userId="me", maxResults=500, pageToken=page,
            q=a.q, includeSpamTrash=True).execute()
        ids += [x["id"] for x in r.get("messages", [])]
        page = r.get("nextPageToken")
        if not page:
            break
    print(f"list: {len(ids)}", file=sys.stderr)
    # уже проиндексированные не тянем повторно (метки обновляются только для --refresh)
    if not a.refresh:
        have = {r[0] for r in c.execute("select id from msg")}
        ids = [i for i in ids if i not in have]
    print(f"качать метаданные: {len(ids)}", file=sys.stderr)

    # messages.get = 5 units; лимит на пользователя считается в минуту, поэтому
    # ходим маленькими пачками с паузой и ретраим то, что упало по квоте.
    pending = ids
    delay = a.sleep
    rnd = 0
    while pending and rnd < 8:
        rnd += 1
        failed = []

        def cb(rid, resp, exc):
            if exc:
                failed.append(rid)
                return
            store(c, resp)

        for i in range(0, len(pending), a.batch):
            chunk = pending[i:i + a.batch]
            batch = svc.new_batch_http_request(callback=cb)
            for mid in chunk:
                batch.add(svc.users().messages().get(
                    userId="me", id=mid, format="metadata", metadataHeaders=[
                        "From", "Subject", "List-Unsubscribe", "List-Unsubscribe-Post"]),
                    request_id=mid)
            try:
                batch.execute()
            except Exception as e:
                print(f"пачка упала: {type(e).__name__}: {str(e)[:100]}", file=sys.stderr)
                failed.extend(chunk)
            c.commit()
            time.sleep(delay)
            print(f"раунд {rnd}: {min(i + a.batch, len(pending))}/{len(pending)} "
                  f"(ошибок {len(failed)})", file=sys.stderr)
        pending = failed
        if pending:
            delay = min(delay * 2, 20.0)
            print(f"повтор для {len(pending)} через {delay:.1f}с", file=sys.stderr)
            time.sleep(delay)
    n = c.execute("select count(*) from msg").fetchone()[0]
    print(f"в индексе {n} писем" + (f", не докачано {len(pending)}" if pending else ""))


def cmd_report(a):
    c = db()
    q = lambda s, *p: c.execute(s, p).fetchall()
    print(f"всего: {q('select count(*), sum(size) from msg')[0][0]} писем, "
          f"{q('select sum(size) from msg')[0][0]//1048576} МБ")
    print(f"непрочитанных: {q('select count(*) from msg where unread=0')[0][0]}")
    print("\n== топ отправителей (по числу писем)")
    for d, n, u, sz in q("""select domain, count(*), sum(unread=0), sum(size)
        from msg group by domain order by 2 desc limit ?""", a.top):
        print(f"{n:5}  unread {u:4}  {sz//1048576:5} МБ  {d}")
    print("\n== топ отправителей (по непрочитанным)")
    for d, n, u in q("""select domain, count(*), sum(unread=0) from msg
        group by domain having sum(unread=0)>0 order by 3 desc limit ?""", a.top):
        print(f"unread {u:4}  всего {n:5}  {d}")
    print("\n== по годам")
    for y, n, u in q("""select strftime('%Y', ts, 'unixepoch') y, count(*), sum(unread=0)
        from msg group by y order by y"""):
        print(f"{y}  {n:5}  unread {u}")
    print("\n== метки")
    for l, n in q("""select l, count(*) from (
        select msg.id, value l from msg, json_each('[\"' || replace(labels,',','\",\"') || '\"]')
        where labels<>'') group by l order by 2 desc limit 20"""):
        print(f"{n:5}  {l}")
    print("\n== топ адресов (по числу писем)")
    for a_, n, u in q("""select from_addr, count(*), sum(unread=0) from msg
        group by from_addr order by 2 desc limit ?""", a.top):
        print(f"{n:5}  unread {u:4}  {a_}")
    print("\n== топ адресов по непрочитанным")
    for a_, n, u in q("""select from_addr, count(*), sum(unread=0) from msg
        group by from_addr having sum(unread=0)>0 order by 3 desc limit ?""", a.top):
        print(f"unread {u:4}  всего {n:5}  {a_}")
    print("\n== старые (до 2024)")
    for y, n in q("""select substr(strftime('%Y-%m', ts, 'unixepoch'),1,7) ym, count(*)
        from msg where ts < strftime('%s','2024-01-01') group by ym order by ym limit 40"""):
        print(f"{y}  {n}")
    print("\n== вложения: крупнейшие домены")
    for d, n, sz in q("""select domain, count(*), sum(size)/1048576 from msg
        group by domain order by 3 desc limit 10"""):
        print(f"{sz:5} МБ  {n:5} писем  {d}")


# ---------- правки ----------

def apply_ids(svc, ids, a):
    """Правка пачкой: batchModify принимает максимум 1000 id за вызов."""
    total = 0
    for i in range(0, len(ids), 1000):
        chunk = ids[i:i + 1000]
        body = {"ids": chunk}
        add, rem = [], []
        if a.trash:
            add.append("TRASH"); rem += ["INBOX", "UNREAD"]
        if a.archive:
            rem.append("INBOX")
        if a.label:
            add.append(resolve_label(svc, a.label))
        if a.unlabel:
            rem.append(resolve_label(svc, a.unlabel))
        if add:
            body["addLabelIds"] = add
        if rem:
            body["removeLabelIds"] = rem
        svc.users().messages().batchModify(userId="me", body=body).execute()
        total += len(chunk)
        print(f"  правлено {total}/{len(ids)}")
    return total


def show_sample(ids, n=25):
    c = db()
    for mid in ids[:n]:
        row = c.execute("select from_addr, subject, strftime('%Y-%m-%d',ts,'unixepoch') "
                        "from msg where id=?", (mid,)).fetchone()
        if row:
            print(f"  {row[2]}  {row[0][:34]:34} {row[1][:60]}")
    if len(ids) > n:
        print(f"  ... ещё {len(ids)-n}")


def cmd_batch(a):
    """batch --query Q --trash|--archive|--label X|--unlabel X [--dry-run] [--limit N]"""
    svc = build()
    ids, page = [], None
    while True:
        r = svc.users().messages().list(userId="me", q=a.query, maxResults=500,
                                        pageToken=page).execute()
        ids += [x["id"] for x in r.get("messages", [])]
        page = r.get("nextPageToken")
        if not page:
            break
    if a.limit:
        ids = ids[:a.limit]
    if not ids:
        print("ничего не найдено"); return
    print(f"под правило попадает {len(ids)} писем")
    if a.dry_run:
        show_sample(ids)
        print("dry-run: ничего не менял")
        return
    print(f"готово: {apply_ids(svc, ids, a)} писем")


def cmd_apply(a):
    """apply --where SQL --trash|--archive|--label X|--unlabel X [--dry-run] [--limit N]

    Отбор по локальному индексу: SQL-фрагмент в WHERE таблицы msg.
    """
    svc = build() if not a.dry_run else None
    c = db()
    sql = f"select id from msg where ({a.where})"
    if a.limit:
        sql += f" limit {a.limit}"
    ids = [r[0] for r in c.execute(sql)]
    if not ids:
        print("по условию пусто"); return
    sz = c.execute(f"select sum(size)/1048576 from msg where id in "
                   f"(select id from msg where ({a.where})"
                   + (f" limit {a.limit})" if a.limit else ")")).fetchone()[0]
    print(f"под правило попадает {len(ids)} писем, {sz or 0} МБ")
    show_sample(ids)
    if a.dry_run:
        print("dry-run: ничего не менял")
        return
    n = apply_ids(svc, ids, a)
    # метки в индексе обновим только на --refresh, но unread/INBOX помечаем сразу
    if a.trash:
        c.execute("update msg set unread=1, labels=labels||',TRASH' where id in ("
                  + ",".join("?" * len(ids)) + ")", ids)
    elif a.archive:
        c.execute("update msg set unread=1 where id in (" + ",".join("?" * len(ids)) + ")", ids)
    c.commit()
    print(f"готово: {n} писем")


def labels(svc):
    return {l["name"]: l["id"] for l in
            svc.users().labels().list(userId="me").execute().get("labels", [])}


def resolve_label(svc, name):
    ls = labels(svc)
    if name in ls:
        return ls[name]
    l = svc.users().labels().create(userId="me", body={
        "name": name, "labelListVisibility": "labelShow",
        "messageListVisibility": "show"}).execute()
    print(f"создана метка {name}")
    return l["id"]


def cmd_filters(a):
    svc = build()
    if a.action == "list":
        for f in svc.users().settings().filters().list(userId="me").execute().get("filter", []):
            print(json.dumps(f, ensure_ascii=False))
        return
    if a.action == "create":
        spec = json.load(open(a.file))
        made = 0
        for it in spec:
            filt = {"criteria": it["criteria"], "action": it["action"]}
            svc.users().settings().filters().create(userId="me", body=filt).execute()
            made += 1
            print(f"+ {it.get('note', it['criteria'])}")
        print(f"создано фильтров: {made}")


def cmd_unsub(a):
    """отписка: List-Unsubscribe-Post (one-click, RFC 8058) либо GET/mailto"""
    svc = build()
    c = db()
    skip = [x.strip() for x in (a.exclude or "").split(",") if x.strip()]
    rows = c.execute("""select id, from_addr, domain, list_unsub, list_unsub_post
        from msg where list_unsub<>'' and unsub_done=0 and (?='' or domain=?)
        group by domain""", (a.domain or "", a.domain or "")).fetchall()
    rows = [r for r in rows if r[2] not in skip]
    print(f"исключено доменов: {len(skip)}")
    print(f"домены с отпиской и без отметки: {len(rows)}")
    for mid, addr, dom, lu, lup in rows:
        urls = re.findall(r"<([^>]+)>", lu or "")
        ok = False
        note = ""
        for u in urls:
            if not u.startswith("http"):
                continue
            if a.dry_run:
                note = ("one-click " if "one-click" in (lup or "").lower() else "get ") + u[:70]
                break
            try:
                if "one-click" in (lup or "").lower():
                    req = urllib.request.Request(u, data=b"List-Unsubscribe=One-Click",
                                                 method="POST")
                else:
                    req = urllib.request.Request(u, method="GET")
                req.add_header("User-Agent", "Mozilla/5.0")
                with urllib.request.urlopen(req, timeout=15) as r:
                    note = f"{r.status} {u[:60]}"
                    ok = r.status < 400
                break
            except Exception as e:
                note = f"{type(e).__name__}: {str(e)[:80]}"
        print(f"{'ok ' if ok else '-- '} {dom:35} {note}")
        if ok and not a.dry_run:
            c.execute("update msg set unsub_done=1 where domain=?", (dom,))
    c.commit()
    if a.dry_run:
        print("dry-run: отметки не ставил")


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = p.add_subparsers(dest="cmd", required=True)

    a = sub.add_parser("auth", help="OAuth: получить url или обменять код на токен")
    a.add_argument("--url", action="store_true")
    a.add_argument("--code")
    a.set_defaults(f=cmd_auth)

    a = sub.add_parser("index", help="собрать/дополнить локальный индекс метаданных")
    a.add_argument("--q", default="")
    a.add_argument("--refresh", action="store_true", help="перекачать и то, что уже в индексе")
    a.add_argument("--batch", type=int, default=10, help="писем в одном batch-запросе")
    a.add_argument("--sleep", type=float, default=1.0, help="пауза между пачками, сек")
    a.set_defaults(f=cmd_index)

    a = sub.add_parser("report", help="сводка по индексу: отправители, годы, метки")
    a.add_argument("--top", type=int, default=40)
    a.set_defaults(f=cmd_report)

    a = sub.add_parser("apply", help="пачка правок по выборке из локального индекса (SQL)")
    a.add_argument("--where", required=True, help="SQL-условие по таблице msg")
    a.add_argument("--trash", action="store_true")
    a.add_argument("--archive", action="store_true")
    a.add_argument("--label")
    a.add_argument("--unlabel")
    a.add_argument("--limit", type=int)
    a.add_argument("--dry-run", action="store_true")
    a.set_defaults(f=cmd_apply)

    a = sub.add_parser("batch", help="пачка правок по запросу Gmail")
    a.add_argument("--query", required=True)
    a.add_argument("--trash", action="store_true")
    a.add_argument("--archive", action="store_true")
    a.add_argument("--label")
    a.add_argument("--unlabel")
    a.add_argument("--limit", type=int)
    a.add_argument("--dry-run", action="store_true")
    a.set_defaults(f=cmd_batch)

    a = sub.add_parser("filters", help="список/создание фильтров Gmail")
    a.add_argument("action", choices=["list", "create"])
    a.add_argument("--file")
    a.set_defaults(f=cmd_filters)

    a = sub.add_parser("unsub", help="отписаться от рассылок через List-Unsubscribe")
    a.add_argument("--domain")
    a.add_argument("--exclude", help="домены через запятую: не отписываться")
    a.add_argument("--dry-run", action="store_true")
    a.set_defaults(f=cmd_unsub)

    a = p.parse_args()
    a.f(a)


if __name__ == "__main__":
    main()
