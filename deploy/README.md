# Юниты surface-сервисов

Копии из `~/.config/systemd/user/` — чтобы конфигурация портов была в репозитории,
а не только на машине.

| юнит | что | порт |
|---|---|---|
| `snens.service` | прод-ядро (ставит `scripts/prod-restart.sh`) | :7420 |
| `snens-web.service` | прод-веб-клиент, vite → `/api` :7420 | :5180 |
| `snens-dev.service` | стенд-ядро, код из worktree, свои данные | :7440 |
| `snens-dev-web.service` | стенд-веб-клиент (vite) | :5175 |

5174 — личный порт человека: прод- и стенд-клиенты его не занимают. Порт веба
задаётся `SNENS_WEB_PORT` в `~/projects/snens-web/vite.config.ts`.

Рестарт прода обрывает активную поверхностную сессию (`done=error`) — делать не
посреди разговора, и `systemctl` отдельно от `prod-restart.sh`.
