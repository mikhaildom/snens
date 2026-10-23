# Деплой: юниты systemd (пример)

Как держать ядро и веб-клиент сервисами. Оба юнита — **пример**, а не обязательное
условие продукта: десктопная сборка запускает ядро сама, без systemd.

| юнит | что | порт |
|---|---|---|
| `snens.service` | ядро, HTTP/SSE (`snens serve`) | :7420 |
| `snens-web.service` | веб-клиент: статика + прокси `/api` в ядро | :5180 |

Порты меняются окружением: `SNENS_WEB_PORT` для клиента, `--listen` (или правка
`ExecStart`) для ядра. Пути в юнитах — через `%h`, чтобы файл переезжал между
машинами без правок.

```sh
mkdir -p ~/.config/systemd/user
cp deploy/snens.service deploy/snens-web.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now snens.service snens-web.service
```

Рестарт ядра обрывает активную сессию поверхности (`done=error`): обновлять его
стоит не посреди разговора. Установка релиза ядра — `scripts/prod-restart.sh`
(immutable-релиз по sha + атомарное переключение launcher).

Отдельный инстанс (стенд, эксперимент) — это не второй бинарник, а свои
каталоги и порт:

```sh
snens serve --config ~/.config/snens-dev/config.toml \
            --data-dir ~/.local/share/snens-dev --listen 127.0.0.1:7440
```
