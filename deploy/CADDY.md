# Что человеку нужно сделать руками (root)

Один домен — `snens.svuvi.ch`: лендинг (`/`), документация (`/docs/*.html`),
плейнтекст для агентов (`/llm/*.txt`), установщики и манифест обновления
(`/releases/*`). DNS на него уже указывает; каталоги `/var/www/snens.svuvi.ch` и
`/var/www/snens.svuvi.ch/releases` создаёт и наполняет `scripts/site-publish.sh` и
`scripts/release.sh` — не хватает только блока в Caddyfile.

Старые `docs.svuvi.ch` и `releases.svuvi.ch` уводятся на него 301-редиректом: в
прошлых версиях десктопа и в промптах живых сессий были зашиты эти адреса, а
редирект дешевле, чем поддерживать три имени одного сайта. Отдельные каталоги под них
больше не нужны.

## Шаги

1. Заменить блок домена в `/etc/caddy/Caddyfile` содержимым
   `deploy/caddy-sites.conf` этого репозитория.
2. `sudo caddy validate --config /etc/caddy/Caddyfile` — проверка синтаксиса.
3. `sudo systemctl reload caddy` — без простоя.
4. В `/etc/hosts` или DNS ничего менять не нужно: имена прежние и новое уже
   указывают на эту машину.

## Почему Cache-Control именно такой

- **`/releases`**: `latest.json` читает апдейтер при каждом старте приложения.
  `max-age=300` — пять минут: новый релиз подхватывается не мгновенно, зато
  Cloudflare (он стоит перед доменом) не отдаёт вечность кешированный манифест
  после выкладки. Установщики под `/<версия>/` неизменяемы, но и их кеш на пять
  минут никому не мешает.
- **`/docs`, `/llm`, `/`**: агент читает эти файлы ходом, документация правится
  часто; минута — чтобы правка доезжала без сброса кеша руками.

## Проверка после reload

```sh
curl -sS https://snens.svuvi.ch/ | head -20
curl -sS https://snens.svuvi.ch/llm/index.txt | head -20
curl -sS https://snens.svuvi.ch/releases/latest.json | head -20
curl -sSI https://docs.svuvi.ch/index.txt | head -3        # 301 на новый домен
```

Первое — лендинг, второе — плейнтекст-оглавление для агентов, третье — манифест
апдейтера (`{"version": ..., "platforms": {"windows-x86_64-nsis": {...}}}`).

## Почему это не сделал агент

`/etc/caddy/Caddyfile` принадлежит root, а `snens-priv` (брокер привилегий) умеет
только `systemctl restart|reload|start|stop|status` для юнитов из whitelist,
`mkdir -p` и `chown` в разрешённых корнях. Правки в `/etc` в whitelist нет
намеренно — это граница, которую агент не обходит.
