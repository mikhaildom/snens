# Surface API (snens-next, ветка feat/surface-api)

Контракт и находки среза 2026-10-06. Рабочий `snens` не затронут: отдельный бинарник,
отдельный data-dir, изоляция проверена тестами (`next.rs::protect_data`).

## Грабли, найденные пробами (не очевидно из кода)

- **PTY-тесты: один общий write всех строк обрезается** канонической дисциплиной
  терминала (~4096 байт на строку с эхом). Отправлять команды с pacing: байты по 2 мс,
  между Enter 250 мс. Не это баг бэкенда — raw paste не отражает клавиатуру.
- **Cargo check/build до добавления `[[bin]]` в Cargo.toml** — бессмысленный шаг, ошибка
  «no bin target» не про код. Сначала манифест, потом проверка сборки.
- **cargo test не пересобирает debug-бинарник**: прогон PTY-проверок по
  `target/debug/snens-next` после правок тестами даёт ложный результат (тестировал
  старый код). Правило: после правок всегда `cargo build --bin snens-next
  --features surface-next` перед внешними прогонами.
- **`fs2` lock держится файлом в data-dir**: второй `serve` на тот же каталог падает
  сразу — это фича, но тестам нужны уникальные data-dir.
- **exec: tool в своей process group** (`process_group(0)` + RAII `kill(-pgid,
  SIGKILL)`). Без этого stop отменял только shell, а `sleep`/компиляторы доживали и
  делали side-эффекты. Родительские detached-агенты используют `setsid` — не ломать.
- **Persistence: сериализация всего store на каждое событие — O(n²)** на длинных
  сессиях. Итог: SQLite (rusqlite bundled), события append-only, state снапшот отдельно.
  `store.json` мигрируется один раз и переименовывается в `.imported`.
- **SSE snapshot+subscribe гонка**: сначала взять блокировку данных, затем
  подписываться, затем слить snapshot и живой поток с фильтром по `seq`. Внешний
  `broadcast` без блокировки пропускает/дублирует события.
- **Broadcast channel 1024**: медленный клиент получит `Lagged` — это не фатально, но
  протокол должен допускать reconnect с курсора, а не деградацию молча.
- **`--token-file auto` — сантинел data-dir**, не относительный путь: иначе секрет
  создавался в cwd запускающего (0600, но место неожиданное). Тест
  `next::tests::token_path_resolves_sentinel_to_data_dir`.
- **SIGTERM не равен SIGINT**: systemd/docker/тесты шлют SIGTERM; `tokio::signal::ctrl_c`
  его не ловит. Оба сигнала через общий обработчик shutdown; на выходе running-сессии
  помечаются `interrupted`, queued-сообщения `cancelled` (не воскресают при рестарте).
- **create idempotency обязан сверять payload**: тот же `command_id` с другим `surface`
  → 409, не тихий replay прежнего ответа.
- **Формулировка скриптов проверок**: tester-агент не должен редактировать код в свою
  пользу (например, ослаблять проверку store.json при смене формата). Изменение
  harness-проверки — решение родителя, не исполнителя.

## Параллельная работа подагентов (родитель ↔ исполнители)

- Общий worktree: каждый агент пишет только в свои файлы; сборки через
  `CARGO_TARGET_DIR=/mnt/HC_Volume_106138989/cargo-target/snens-next`, чтобы не
  пересекаться и не трогать рабочий target.
- Родитель делает интеграцию (Cargo.toml, mod-декларации, entrypoint) сам и не даёт
  подагентам `git commit` — иначе конфликты и полусобранные состояния.
- Направляющие сообщения: конкретные находки из внешней проверки (например,
  независимый tester) пересылать исполнителю дословно, без пересказа.

## Что проверено живьём (Nous, не мок)

Стрим дельт и тул-события, steer во время хода, Ctrl+C (run.cancelled, потомки убиты),
`/sessions` → `/resume`, SIGTERM graceful, токен из `.env` production найден сам через
`cfg.api_key()`. Отдельная экспериментальная ветка, не в master.

## History and legacy session compatibility

`GET /v1/sessions` returns `{sessions:[{id,surface,state,run_id}]}` merged from existing API delivery state and UUID-or-legacy `<timestamp>-<pid>.jsonl` files under the configured sessions directory. Legacy logs without `Meta.surface` use `surface: "legacy"`; discovery is read-only and does not migrate/rewrite transcripts. `GET /v1/sessions/{id}` resolves a valid legacy JSONL ID even before it has API delivery state.

`GET /v1/sessions/{id}/history` returns `{cursor,events,messages}`. `cursor` and `events` retain the API's durable SSE-delivery sequence and lifecycle/action events; they are not transcript positions. `messages` is the canonical JSONL projection, ordered by physical 1-based line number: `{seq,ts,role,content,source?,reasoning?,tool_calls?,tool_call_id?,name?,exit?}`. `role` is user/assistant/tool; seq may have gaps for metadata/accounting records. Assistant calls use OpenAI function-call shape. No transcript deltas/messages are synthesized into SSE. Complete malformed JSONL records fail the history request; an incomplete malformed final tail is ignored. Resuming a JSONL session opens its exclusive writer lock; concurrent writers fail fast.

## Экспериментальная установка для разработки

`~/projects/snens-next` — git worktree ветки `feat/surface-api`, отдельный от рабочего
`~/projects/snens` (общая git-история, разные рабочие деревья). Все новые изменения сначала
собираются и проверяются здесь; перенос и обновление рабочей установки — отдельно.

- Конфиг `~/.config/snens-next/config.toml`, собственная приватная копия `.env`.
- Данные `~/.local/share/snens-next`: отдельные sessions/mail/tickets/store, копии
  soul/user/skills и собственная memory.md. Это отдельный локальный git-репозиторий;
  ключи, токены, сессии и runtime-конфиг исключены из git.
- Тулы собираются из этого worktree в отдельный cargo target и устанавливаются в
  его `tools/*`; staging и PATH не используют бинарники рабочего worktree.
- `snens-next` (без аргументов) — TUI; `snens-next tui --session ID` — восстановить сессию.
- User-service `snens-next.service` держит HTTP/SSE сервер на loopback:7419.
  `systemctl --user status|restart|stop snens-next`, логи `journalctl --user -u snens-next`.
  Перед рестартом завершить ходы: running станут interrupted, queued — cancelled.
- `snens-next serve` — ручной запуск с экспериментальным конфигом. Сначала остановить
  service; второй сервер на том же data-dir запрещён.
- `snens-next dev build` — только экспериментальное ядро и тулы.
- `snens-next dev verify` — сборка, workspace tests, clippy, HTTP/PTY smoke и tool selftests.
- `snens-next dev install` — сборка и launcher в `~/.local/bin/snens-next`.

Автоматическая memory и review отключены, тестовая модель — `openai/gpt-6-luna`.
Это **не sandbox**: инструменты имеют обычные права пользователя; явный вызов memory
может обращаться к общей внешней памяти, его не использовать для тестовых записей.
В surface API есть session-owned attachment upload; remote secret/sudo entry пока нет.
Сервис не подхватывает пересборку сам: после успешных тестов нужен restart.

## Session-scoped outbound tools

Adapter registers OpenAI function definitions on an existing session with authenticated `PUT /v1/sessions/{sid}/tools`, body `{"tools":[...]}`; response is `{"tools":[...]}`. Repeating the same definitions is safe while a run is active; changing them while active returns 409. Definitions are session-local, capped at 8 and 32 KiB, and may not shadow core tools.

For a model call, SSE emits `tool.requested` with the ordinary `seq`, `session_id`, `run_id`, and `type` envelope plus string `call_id`, `name`, and `args` (raw JSON argument string). The adapter posts `{"text":"...","exit":0}` to `/v1/sessions/{sid}/runs/{rid}/tools/{call_id}/result`; a first submission and exact retry return 200 `{"accepted":true}`. Unknown calls return 404; expired/cancelled calls and conflicting retries return 409. Result text is capped at 64 KiB. Stop cancels the waiter; a late result is rejected. Tools have a finite 60-second wait; timeout becomes a normal failed tool result. No callback URL is accepted and the core never initiates arbitrary outbound requests. These tools exist only on the HTTP surface; the terminal TUI continues to use its regular core tool set.

## Модель: `/v1/model`

`GET /v1/model` → `{"model":"<slug>"}`; `PUT /v1/model` с `{"model":"<slug>"}`
меняет основную модель (та же операция, что `snens model <slug>`) и возвращает
`{"model":"<slug>","config":"<path>"}`. Пустой slug — 400.

Зачем эндпоинт: сервер — долгоживущий процесс со снимком конфига, а `snens model`
правит только файл. Поэтому `agent::run_loop` на каждом ходу перечитывает `model`
из конфига (`Config::model_now`, `source_path` хранится в снимке), а PUT пишет в
тот же файл, что читает процесс (учитывает `--config`). Без этого новый ход уходил
бы моделью, снятой при старте сервиса.

Подагент (`run-child`) подменяет модель на `subagent_model` — это помечено
`model_pinned`, и перечитка файла её не трогает (файл про основную модель).

## SSE: изоляция и переполнение

Снимок событий и подписка берутся под общей блокировкой с emit. Каждая подписка
отдаёт только события своей сессии с `seq > cursor`, включая live-поток.
При переполнении broadcast буфера история после последнего отправленного cursor
восстанавливается из persisted store; остаток broadcast дедуплицируется.
TUI не двигает cursor от чужих событий и не рисует повторы.
Детерминированные unit-тесты проверяют переполнение >1024 и границу snapshot/live;
HTTP-регрессия проверяет две подписки с разными per-session seq.

Публикация SurfaceSignal идёт через один FIFO relay; перед assistant.completed и
run.finished сервер ждёт его опустошения. Независимые tokio::spawn на каждый delta
переупорядочивали чанки — это поймала PTY-проверка queued steer после reconnect.

Финальная проверка экспериментальной установки: полный `snens-next dev verify`
(86 next unit-тестов, workspace tests/clippy, 27 HTTP/PTY checks, tool selftests),
повторный smoke 27/27; реальный Nous/Luna через установленный TUI PTY: run printf,
ответ/стрим, sleep+Ctrl+C, /quit, восстановление истории после restart service.
Production binary и config совпали по SHA-256 до/после.

Изоляция data-dir проверяется до mkdir (включая существующие symlink-предки).
Ключ исходного конфига разрешается до переключения на runtime.toml и наследуется
дочерними процессами; ключ не записывается в runtime.toml/store/log.
Incremental cache в next-worktree выключен: target-диск общий и заполнен почти
полностью; удаление next/debug/incremental освободило 1.3 ГБ. Production target не чистили.

## Входящие вложения

`POST /v1/sessions/{id}/attachments?filename=NAME&command_id=UUID` с Bearer и
raw bytes (`application/octet-stream`). Ответ 201 (200 на точный retry):
`{attachment_id,name,path,mime,bytes}`. Путь абсолютный, UUID-файл принадлежит
ядру, каталог приватный. Лимит — `max_attachment_bytes`; превышение 413.
Имя без `/` и `\`, изображения проверяются по заголовку. Остальные файлы доступны
модели через путь/инструменты, а не как image. Один upload command ID с другим
именем/байтами — 409. Metadata и ссылки сообщений сохраняются в SQLite.

`POST /v1/sessions/{id}/messages`: `attachment_ids: []` опционально;
пустой текст допустим с вложениями. Чужой/неизвестный ID — 400. Повтор message
command ID обязан совпадать по text и attachment IDs. Busy-очередь сохраняет
parts; `Rec::User` backward-compatible (отсутствующие parts = []). Изображение
проецируется как `image_url`, документ — текстовая строка с path/MIME/bytes.

Проверка Telegram: `scripts/media-smoke.py` в отдельном репозитории адаптера
поднимает настоящее ядро с mock LLM и mock Telegram; реальные ключи не нужны.
