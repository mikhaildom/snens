# Промт: перенести snens на Windows 7

Этот текст можно целиком отдать агенту (snens, Claude Code и т. п.), который работает в клоне `snens` и `snens-web`. Готовые правки лежат в ветке `win7` репозиториев
[mikhaildom/snens](https://github.com/mikhaildom/snens/tree/win7) и [mikhaildom/snens-web](https://github.com/mikhaildom/snens-web/tree/win7). Их можно забрать целиком или повторить по шагам ниже, если `master` уже ушёл вперёд.

---

## Задача

Сделай так, чтобы snens собирался и работал на **Windows 7 SP1 x64** без дополнительных обновлений. Проверочная машина: AMD E-300 (2 ядра, без SSE4.1/AVX), 4 ГБ, без KB2533623 и KB3125574.

Нужны три способа запуска:

1. `snens-desktop.exe` — настоящее окно (Tauri, `desktop/`).
2. `snens-app.exe` — окно, которое рисует Chrome 109 / Supermium / Edge 109.
3. `start-snens.cmd` — ядро в консоли плюс обычный браузер.

Возьми за образец ветку `win7` (см. выше): там всё это уже сделано и проверено на живой машине. Ниже — что именно ломается и как это чинить.

## 1. Ядро и инструменты (`snens`)

- **Таргет.** Собирай под `x86_64-win7-windows-gnu`. Это tier 3, поэтому нужен nightly и `-Z build-std=std,panic_abort`:
  - тулчейн: `rustup toolchain install nightly-x86_64-pc-windows-gnu --profile minimal -c rust-src`;
  - C-компилятор: [w64devkit](https://github.com/skeeto/w64devkit) — нужен для ring, sqlite, sqlite-vec, tree-sitter и для линковки.
- **libgcc_eh.** В w64devkit нет `libgcc_eh.a`, а rustc всегда просит `-lgcc_eh`. Подложи пустой архив (`ar rcs libgcc_eh.a`) через `LIBRARY_PATH`.
- **Флаги.** `RUSTFLAGS="-C target-cpu=x86-64 -C link-args=-static"`.
  - Никакого `target-cpu=native`: у целевых машин нет SSE4/AVX.
  - `-static` нужен, чтобы рядом с exe не требовались DLL MinGW.
  - Готовый скрипт: `scripts/win7/build.sh`.
- **Цвет в консоли.** Консоль Win7 не понимает ANSI-цвета: `ENABLE_VIRTUAL_TERMINAL_PROCESSING` появился только в Windows 10. Проверь `SetConsoleMode` и при отказе печатай без цвета (`src/render.rs`, `ansi_supported()`). Иначе в тексте будет мусор вида `←[36m`.
- **Проверка импортов.** Каждый exe и DLL должен импортировать только то, что есть в Windows 7. Скрипт `scripts/win7/check-imports.sh` (objdump) проходит по всем бинарникам и падает на лишнем импорте. Запускай его после каждой сборки.

## 2. Окно приложения (`desktop/`, Tauri 2 / wry / webview2-com)

- **Движок.** Последний WebView2 для Win7 — **109**. Системно он больше не ставится, поэтому вези его с программой:
  - папка `WebView2\` рядом с exe;
  - взять из автономного установщика Edge 109 x64 в каталоге Центра обновления Microsoft: `MSEDGE.7z → Chrome-bin\109.0.1518.140`.
- **desktop/src/main.rs.** До создания окон: если рядом с exe есть `WebView2\msedgewebview2.exe`, задай `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`. Функция `use_bundled_webview2()`.
- **Заплатка `webview2-com`.** `target_compatible_browser_version` по умолчанию равна версии SDK (145+), и загрузчик отвергает движок 109. Поставь `"109.0.0.0"`.
- **Заплатка `wry`.** Передавай папку движка из `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` и в `CreateCoreWebView2EnvironmentWithOptions`, и в `GetAvailableCoreWebView2BrowserVersionString`. С пустой папкой загрузчик ищет только установленный движок.
- **Подключение заплаток.** Обе заплатки — копии крейтов в `scripts/win7/patches/`. Подключаются через `--config patch.crates-io.<crate>.path=...` в `scripts/win7/build-desktop.sh`. `desktop/Cargo.lock` после сборки возвращается как был.
- **Заменители DLL Windows 8** (`scripts/win7/w7compat/`):
  - `combase.dll` переадресует `CoTaskMemAlloc`, `CoTaskMemFree` и `CoCreateFreeThreadedMarshaler` в `ole32`;
  - `api-ms-win-core-winrt-error-l1-1-0.dll` содержит `RoOriginateErrorW`, который возвращает FALSE.
  - Без них exe не стартует.
- **WebView2Loader.dll.** Он грузит `version.dll`, `advapi32` и `shell32` через `LoadLibraryExW(..., LOAD_LIBRARY_SEARCH_SYSTEM32)`. Без KB2533623 Windows 7 этот флаг не понимает. Обновления для Win7 в каталоге Microsoft уже нет, поэтому загрузчик отвечает `0x80070032`, и окно пишет «Could not find the WebView2 Runtime».
  - Чинит `scripts/win7/patch-loader.py`: ставит флаг 0 в этих четырёх вызовах.
  - `stage.sh` кладёт загрузчик в поставку уже через этот скрипт.

## 3. Запускалка `snens-app.exe` (`scripts/win7/app/`)

- Отдельный маленький Rust-бинарник. Поднимает ядро, получает код подключения (`snens.exe pair`, таймаут 15 с) и открывает окно `--app=` в Chrome 109 / Supermium / Edge 109. Использует отдельный профиль браузера.
- История профиля чистится до запуска и после: код подключения не должен оставаться в истории.
- Ядро гасится вместе с окном, если профиль больше никто не держит.

## 4. Веб-клиент (`snens-web`)

- **Цель сборки.** В `vite.config.ts` задай явно: `build: { target: ['chrome109', 'firefox115', 'edge109', 'safari16'] }`. Иначе Vite 8 выдаёт синтаксис новее Chrome 109, и страница остаётся белой.
- **Полифилы.** `src/lib/legacy-polyfills.ts`, первым импортом в `main.tsx`: `Object.groupBy`, `Map.groupBy`, `Promise.withResolvers`. Без них mermaid/langium падают в Chrome 109.
- **Запасные цвета.** `src/styles/32-old-browsers.css`: блок `@supports not (color: color-mix(...))`. Без `color-mix` (он есть только с Chrome 111) пропадают подложки, клетки карты активности и полоса контекста.
- **Вход по коду.** Код подключения можно передать в адресе `#pair=…` (`src/pair/launchCode.ts`). Код забирается и сразу стирается из адресной строки.
- **Слабые машины.** Если `navigator.hardwareConcurrency <= 2`, `reducedMotion()` в `src/ui/dither.ts` возвращает true, и пиксельные поля рисуются одним неподвижным кадром. Живое поле на E-300 в простое занимало ~80% процессора.

## 5. Поставка и проверка

- `scripts/win7/stage.sh [dist snens-web] [выход]` собирает папку `snens/`:
  - exe и тулы, `web/`;
  - `snens-desktop.exe` и загрузчик WebView2 (уже поправленный);
  - заменители DLL, `start-snens.cmd` (в кодировке CP866), `README.txt`.
- Движок добавляется, если задать `WEBVIEW2_DIR`.
- Проверяй на настоящей Windows 7, а не на Windows 10/11: там всё это работает и без правок, поэтому проблем не видно.
  - `check-imports.sh` должен выдавать `ok` для всех файлов.
  - Окно должно открыться.
  - Процессор в простое при открытом окне — единицы процентов.

Ничего не меняй в поведении для новых Windows: каждая правка либо стоит за проверкой «нет такой возможности», либо включается только для Win7-поставки.
