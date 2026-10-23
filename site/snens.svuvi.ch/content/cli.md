# Команды

`snens` без подкоманды печатает справку. Общие флаги: `--config PATH` (другой
конфиг), `--session ID` (конкретная сессия вместо последней), `--endpoint URL` и
`--token-file PATH` (для команд, которые идут в запущенное ядро). Код возврата — 0
при успехе и 1 при ошибке; текст ошибки в stderr.

## Работа

| команда | что делает | пример |
|---|---|---|
| `snens run` | один цикл до ответа модели | `snens run "собери проект"` |
| `snens run --new` | то же в новой сессии | `snens run --new "начнём с нуля"` |
| `snens run --detach` | подагент отдельным процессом, печатает его id | `snens run --detach --session s1 "изучи репозиторий"` |
| `snens steer` | послать работающему подагенту сообщение; id — во флаге `--session` | `snens steer --session s1 "хватит, итог"` |
| `snens agents` | список детей сессии и их состояние | `snens agents --session s1` |
| `snens result` | финальный текст сессии | `snens result s1` |
| `snens review` | ревью сессии в тикеты | `snens review --session s1` |
| `snens improve` | сессия улучшения харнесса в репозитории исходников | `snens improve` |

`run` без `--session` продолжает последнюю сессию. `steer` берёт текст из argv или
со stdin; в справке id указан аргументом, а читается из `--session` — без флага
команда завершится ошибкой. `review` работает по уже записанному журналу; `improve`
требует `tools_dir` внутри git-репозитория.

## Поверхности

| команда | что делает | пример |
|---|---|---|
| `snens serve` | HTTP/SSE-ядро, при первом старте создаёт токен | `snens serve` |
| `snens serve --listen ADDR` | адрес и порт ядра | `snens serve --listen 127.0.0.1:8080` |
| `snens serve --data-dir DIR` | отдельный инстанс: свои сессии, ключ, токен | `snens serve --data-dir ~/.local/share/snens-2` |
| `snens tui` | TUI-клиент к запущенному ядру | `snens tui --endpoint http://127.0.0.1:8080` |
| `snens tui-local` | TUI в этом же процессе, без сервера и токена | `snens tui-local` |
| `snens pair` | код на 5 минут и адреса для приложения | `snens pair` |
| `snens clients` | подключённые устройства | `snens clients` |
| `snens clients revoke <id>` | отозвать устройство, токен перестаёт работать сразу | `snens clients revoke 3f2a` |

`serve` слушает только loopback. Не-loopback адрес требует `--allow-remote` и
приватной сети (tailnet/WireGuard/LAN): API доступен по сети с одним bearer-токеном,
без TLS. Токен: `<данные>/api.token`, `0600`, не короче 32 символов без пробелов —
иначе старт падает.

## Сессии

| команда | что делает | пример |
|---|---|---|
| `snens sessions` | id всех сессий | `snens sessions` |
| `snens show` | токены, кеш, цена, самый крупный результат | `snens show --session s1` |
| `snens log` | журнал сессии, по записи JSON на строку | `snens log --session s1` |
| `snens sweep` | убрать мёртвое: дампы, зеркала, вложения закрытых сессий | `snens sweep --older-than 14 --dry-run` |

`sweep` без `--dry-run` удаляет; `--attachments` добавляет к уборке вложения
(по умолчанию они не трогаются — это данные человека). Порог возраста по умолчанию
— 30 дней.

## Настройка

| команда | что делает | пример |
|---|---|---|
| `snens config` | текущие правимые ключи и путь файла | `snens config` |
| `snens config ключ=значение` | правка конфига на ходу | `snens config max_turns=100` |
| `snens model` | какая модель сейчас | `snens model` |
| `snens model <slug>` | сменить модель в конфиге | `snens model имя-модели` |
| `snens providers` | профили, активный, задан ли ключ | `snens providers` |
| `snens providers activate <имя>` | сделать профиль активным | `snens providers activate cloud` |
| `snens providers key <имя> <ключ>` | положить ключ в `.env` (`-` — со stdin) | `snens providers key cloud -` |
| `snens memory` | состояние канала памяти | `snens memory` |
| `snens memory on` / `off` | включить или выключить канал | `snens memory off` |
| `snens memory recall always` (или `session`, `off`) | когда впрыскивать справку | `snens memory recall session` |
| `snens memory retain on` / `off` | писать ли ход в банк | `snens memory retain off` |

Список ключей для `snens config` — [конфиг](/docs/config.html). `model` при
активном профиле отвечает его моделью: менять её надо в профиле.

## Тикеты

| команда | что делает | пример |
|---|---|---|
| `snens tickets` | открытые тикеты, сначала частые | `snens tickets` |
| `snens tickets --all` | включая закрытые; `--target`, `--kind`, `--status`, `--family` — фильтры | `snens tickets --all --kind bug` |
| `snens tickets show <имя>` | тело тикета по префиксу имени | `snens tickets show 2026-01` |
| `snens tickets grep <текст>` | поиск по имени и телу | `snens tickets grep таймаут` |
| `snens tickets stats` | счёт по статусам и target | `snens tickets stats` |

Неоднозначный префикс — ошибка со списком подходящих, а не первый попавшийся.

## Разработка

| команда | что делает | пример |
|---|---|---|
| `snens tools` | что видит модель: встроенные и реестр | `snens tools` || `snens tools --json` | определения инструментов как они уходят модели | `snens tools --json` |
| `snens build` | собрать весь workspace и разложить бинарники инструментов | `snens build` |
| `snens build read edit` | собрать только названные | `snens build read` |
| `snens selftest` | один ход на фейковой модели: реестр, argv, stdin, PATH, проекция | `snens selftest` |
| `snens verify` | `cargo check` → `cargo test --workspace` → selftest инструментов, один вердикт | `snens verify` |

`verify` останавливается на первом красном шаге и печатает `verify ok` в конце;
репозиторий берётся как родитель `tools_dir`. `selftest` работает только на unix.
