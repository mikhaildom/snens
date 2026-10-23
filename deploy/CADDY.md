# Что человеку нужно сделать руками (root)

Два домена: `releases.svuvi.ch` (автообновление десктопа) и `docs.svuvi.ch`
(первичный источник документации для агентов). DNS у обоих уже стоит, каталоги
созданы, файлы выложены — не хватает только блоков в Caddyfile.

## Шаги

1. Добавить в конец `/etc/caddy/Caddyfile` (блоки лежат готовыми в
   `deploy/caddy-sites.conf` этого репозитория):

   ```
   releases.svuvi.ch {
       root * /var/www/releases.svuvi.ch
       file_server
       header ?Cache-Control "max-age=300"
   }

   docs.svuvi.ch {
       root * /var/www/docs.svuvi.ch
       file_server
       header ?Cache-Control "max-age=60"
   }
   ```

2. `sudo caddy validate --config /etc/caddy/Caddyfile` — проверка синтаксиса.
3. `sudo systemctl reload caddy` — без простоя.

## Почему Cache-Control именно такой

- **releases**: `latest.json` читает апдейтер при каждом старте приложения.
  `max-age=300` — пять минут: новый релиз подхватывается не мгновенно, зато
  Cloudflare (он стоит перед доменом) не отдаёт вечность кешированный манифест
  после выкладки. Сами установщики под `/<version>/` неизменяемы, но и их кеш
  на пять минут никому не мешает.
- **docs**: агент читает эти файлы ходом; `max-age=60` — минута, чтобы правка
  документации доезжала до агента без сброса кеша руками.

## Проверка после reload

```sh
curl -sS https://docs.svuvi.ch/index.txt | head -20
curl -sS https://releases.svuvi.ch/snens/latest.json | head -20
```

Первое должно отдать plaintext-оглавление, второе — манифест апдейтера
(`{"version": ..., "platforms": {"windows-x86_64-nsis": {...}}}`).

## Почему это не сделал агент

`/etc/caddy/Caddyfile` принадлежит root, а `snens-priv` (брокер привилегий)
умеет только `systemctl restart|reload|start|stop|status` для юнитов из
whitelist, `mkdir -p` и `chown` в разрешённых корнях. Правки в `/etc` в
whitelist нет намеренно — это граница, которую агент не обходит.
