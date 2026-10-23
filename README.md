# snens

Агент-харнесс: получает задачу, сам выбирает инструмент, вызывает его и показывает
результат. Ядро — один бинарь на Rust; инструменты — отдельные программы с
контрактом argv и `--help` (его же ядро отдаёт модели как схему).

Статус: личный агент автора, публикуется как есть, без обещаний совместимости.

## Структура

| путь | что |
|---|---|
| `src/` | ядро: цикл «модель → тул → результат», HTTP API и SSE, TUI, журнал сессий, контекст, память, подагенты |
| `tools/` | инструменты, по крейту на тул: `read`, `edit`, `write`, `grep`, `fetch`, `search`, `shot`, `view`, `web`, `desk`, `agent`, `audit`, `memory` |
| `desktop/` | окно на Tauri 2, ядро sidecar'ом |
| `site/` | сайт и документация: `content/*.md` — страницы, `llm/*.txt` — плейнтекст для агентов |
| `docs/` | устройство подсистем: API поверхности, контекст, память, подагенты, пейринг |
| `prompts/` | системные инструкции ревью и improve |
| `scripts/` | релиз десктопа, публикация сайта, обслуживание |
| `deploy/` | unit'ы systemd, блоки Caddy, пример правила sudo |

## Быстрый старт

```sh
cargo build --release --workspace        # ядро и все инструменты
printf 'SNENS_API_KEY=sk-...\n' > ~/.config/snens/.env
printf 'base_url = "https://провайдер.example/v1"\nmodel = "имя-модели"\n' > ~/.config/snens/config.toml
snens build && snens run "покажи, что лежит в текущем каталоге"
snens tui
```

`snens` — бинарь `target/release/snens` из `PATH`. Каталог `~/.config/snens`
создать заранее, `.env` — `chmod 600`. `base_url` без `/chat/completions`; имя
переменной с ключом задаёт `key_env`.

`snens build` кладёт бинарники тулов в `tools/<имя>/<имя>`; чтобы он нашёл
workspace, в `config.toml` нужен `tools_dir = "<клон>/tools"`.

| команда | что |
|---|---|
| `snens run [--new] "запрос"` | ответ в терминале, без флагов — продолжение последней сессии |
| `snens serve` | ядро для клиентов, loopback |
| `snens tui` | диалог с ядром в терминале |
| `snens log` / `show` / `sessions` | журнал сессии, его стоимость, список |
| `snens help` | остальные подкоманды |

## Ссылки

| что | где |
|---|---|
| сайт | https://snens.svuvi.ch/ |
| документация для агентов (плейнтекст) | https://snens.svuvi.ch/llm/index.txt |
| релизы десктопа | https://snens.svuvi.ch/releases/ |
| веб-клиент | https://git.svuvi.ch/svuvich/snens-web |
| плагин памяти | https://git.svuvi.ch/svuvich/snens-memory |

Правки в ядре проверяются `cargo test`; интеграционный цикл — `snens verify`
(`check`, `test`, `selftest` одним вердиктом).

## Лицензия

MIT, см. [LICENSE](LICENSE). Плагины и сторонние скиллы несут свою лицензию.
