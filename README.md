# snens-memory

Долгая память агента snens: один бинарь, один SQLite-файл, без демона и без Postgres.

Плагин, а не сервис: ядро знает только argv-контракт (`memory recall <текст>`,
`memory retain` + JSON хода на stdin), всё остальное — здесь.

Лицензия — MIT (см. `LICENSE`). Контракт намеренно узкий, поэтому плагин
прикручивается к любому харнессу, умеющему позвать программу с argv и stdin `{"user","assistant"}`.

## Быстрый старт

```sh
cargo build --release              # бинарь target/release/memory
mkdir -p ~/.config/snens ~/.local/share/snens
cat > ~/.config/snens/memory.toml <<'TOML'
db = "~/.local/share/snens/memory.db"
api_base = "https://inference-api.nousresearch.com/v1"   # любой OpenAI-совместимый
embed_model = "baai/bge-m3"
distill_model = "deepseek/deepseek-v4.1-flash"
rerank_model = "nvidia/llama-nemotron-rerank-vl-1b-v2:free"  # rerank = "off" чтобы выключить
TOML
echo 'SNENS_API_KEY=...' >> ~/.config/snens/.env            # ключ эмбеддингов/LLM
echo 'SNENS_OPENROUTER_KEY=...' >> ~/.config/snens/.env     # ключ реранкера, если он у другого провайдера

memory retain < ход.json     # {"user":"…","assistant":"…"} → очередь
memory flush                  # очередь → факты (LLM + эмбеддинги)
memory recall "запрос"        # → строки фактов, пусто = нечего сказать
```

Пути зашиты как у snens (`~/.config/snens/memory.toml`, ключ из `.env` рядом);
другой харнесс либо кладёт конфиг туда же, либо правит `cfg::config_path`.
Провайдер нужен OpenAI-совместимый: `/embeddings`, `/chat/completions`, реранк —
`/rerank` (OpenRouter/Cohere-совместимый) или `rerank = "off"` вовсе.
Имя переменной с ключом переопределяется в конфиге (`api_key_env`).

## Зачем

Hindsight (то, что стояло раньше) инжектил справку размером ~2000 токенов в
системный префикс, и она оставалась там до конца сессии: 614 таких записей в
наших сессиях — 1.25M токенов, впечённых в префиксы. Плюс Postgres + pgvector +
Python-контейнер на 383 МиБ.

Здесь решены оба: справка живёт ровно один ход (правка в ядре), а память — файл.

## Как работает

Чтение (`recall`) — без LLM на пути:

```
запрос → BM25 (FTS5) ∪ вектор (sqlite-vec, int8) → RRF(k=20) → реранк → порог → ≤N фактов
```

- **BM25** — FTS5, `unicode61`. Находит точные имена (Xray, 3x-ui, NLB-N11): этого
  эмбеддинги не умеют.
- **Вектор** — `bge-m3` (1024, мультиязычный) через API, хранится `int8` — 1 байт на
  измерение.
- **RRF** — склейка по рангам, не по скорам: шкалы BM25 и косинуса несравнимы.
- **Реранк** — cross-encoder (`cohere/rerank-v3.5`, `nvidia/nemotron-rerank-1b:free`).
  Единственная стадия, умеющая сказать «похоже, но не отвечает».
- **Порог** — относительный (0.4×лучшего скора) плюс нижняя граница. Абсолютный
  порог не переносим между реранкерами: замерено на живом корпусе, nemotron даёт
  релевантному 0.03…0.89, cohere-4-fast — 0.49 при мусоре 0.39.
- **Пустая выдача — законный исход.** Один нерелевантный документ роняет точность
  сильнее, чем помогает случайный (arXiv:2401.14887).

Запись (`retain` → `flush`) — отдельно от чтения:

- `retain` только кладёт ход в очередь (локальная вставка, ноль сети): ядро зовёт
  его после каждого ответа и ждать не должно.
- `flush` — по крону/вручную: модель извлекает 0–5 фактов (`fact|preference|episode|procedure`
  × `subject(user|world|project)`), дедуп по векторам (dist < 0.15), перекрытие —
  через `supersedes`: старый факт получает `valid_to`, но остаётся в таймлайне.

Типы — минимум: `fact`, `preference`, `episode`, `procedure`; субъект — `user`,
`world`, `project`. Всё остальное — атрибуты в тексте.

## Команды

```
memory recall <запрос>     # строки фактов; пусто = нечего сказать
memory retain              # JSON {"user","assistant"} на stdin → очередь
memory flush               # разобрать очередь (LLM + эмбеддинги)
memory stats               # живые/устаревшие факты, очередь, типы
memory reindex             # досчитать вектора (если провайдер лежал)
memory import              # разовый импорт: {"units":[{text,kind,subject}]}
memory forget <uid>        # закрыть факт по uid
```

`MEMORY_DEBUG=1` печатает в stderr скоры кандидатов до отсева.

## Настройки

`~/.config/snens/memory.toml`:

```toml
db = "~/.local/share/snens/memory.db"
api_base = "https://inference-api.nousresearch.com/v1"
embed_model = "baai/bge-m3"
distill_model = "deepseek/deepseek-v4.1-flash"
rerank_model = "nvidia/llama-nemotron-rerank-vl-1b-v2:free"  # rerank = "off" чтобы выключить
max_facts = 6
max_tokens = 400
```

Ключ: `SNENS_API_KEY` (env или `~/.config/snens/.env`), реранкер — `SNENS_OPENROUTER_KEY`.

## Миграция

Старый банк Hindsight → этот файл одним `import` (транзакция, даты `created`
сохранены, дубли по нормализованному тексту отсеяны — в старом банке 125 групп
копий). Сделано: **11511 из 11637** фактов, файл 25 МБ.

```sh
docker exec hindsight-shared-pg psql -U hindsight -d hindsight -tAc "
  select json_build_object('units', json_agg(u)) from (
    select text,
      case fact_type when 'experience' then 'episode' else 'fact' end as kind,
      case when fact_type='experience' then 'project' else 'world' end as subject,
      extract(epoch from created_at)::bigint as created
    from memory_units where length(btrim(text)) > 0 order by created_at) u;" \
  > /tmp/export.json
memory import < /tmp/export.json          # ~9 мин: 11.5k эмбеддингов по сети
```

Повторный импорт отказывается (`уже импортировано N`), принудительно — `--replace`.

## Замеры (на этом хосте, 11.5k фактов после миграции)

| операция | время |
|---|---|
| `recall` целиком | 1.1–1.5 с |
| из них локально (BM25 + вектор + RRF + дедуп) | ~15 мс |
| из них эмбеддинг запроса (сеть, Nous) | 0.9–2.0 с |
| `stats` | 0.07 с |
| RSS процесса | 7–8 МБ |
| база | 25 МБ на 11.5k фактов |

Латентность упирается в **сеть к эмбеддеру**, не в локальную часть — отсюда
следующий шаг: локальные модели (`docs/local-models.md`).

## Замеры (до миграции, 300 фактов)

| операция | время |
|---|---|
| `recall` (эмбеддинг + BM25 + вектор + реранк) | 0.65–0.76 с |
| из них без сети (BM25 + вектор) | ~15 мс |
| RSS процесса | 6.9 МБ |
| `flush` одного хода (1 эмбеддинг + 1 LLM + 2 эмбеддинга) | 4.1 с |
| бинарь (release, strip) | 3.6 МБ |

Качество на 11 запросах (6 информативных + 5 пустых): precision 0.50, recall 0.83,
в среднем 1.1 факта на ход. Раньше было 30–39 фактов и ~10000 символов.

## Чего здесь намеренно нет

- графа сущностей (выигрывает только на multi-hop; индексация ×20–50);
- битемпорального графа (хватает `valid_from`/`valid_to`/`supersedes`);
- decay-забывания (старое ≠ неверное);
- LLM-судьи на чтении (cross-encoder дешевле на порядок при том же качестве
  отсева — arXiv:2601.14224);
- демона (эмбеддинг запроса — 0.5 с сети, локальная работа — 15 мс).
