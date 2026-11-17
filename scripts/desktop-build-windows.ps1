# Собрать подписанный релиз десктопа на Windows: стейдж + бандл.
#
# Зачем одним скриптом. Ночью бандл собрали, а `desktop-stage.sh` больше не
# звали: ядро, тулы и КЛИЕНТ в `desktop/resources/` остались от первой
# раскладки. В установщик уехал устаревший веб-клиент — дашборд в нём падал с
# "e is not iterable", и человек видел пустую страницу. Стейдж обязан быть
# частью релиза, а не отдельным шагом, который можно забыть.
#
# Подводные камни, из-за которых сборка раньше вставала навсегда:
#  1. в cmd `set VAR=` УДАЛЯЕТ переменную, а не задаёт пустую строку — tauri
#     уходит в интерактивный промпт пароля, которого нет в неинтерактивной
#     сессии;
#  2. `TAURI_SIGNING_PRIVATE_KEY_PATH` не подхватывается — передаём СОДЕРЖИМОЕ
#     ключа в `TAURI_SIGNING_PRIVATE_KEY` (так делает и CI).
#
# Прогресс tauri пишет в stderr, поэтому `*>` и мягкий режим ошибок: иначе
# PowerShell валит сборку на первой же строке "Info …".
param(
  # Репозиторий обязателен: раньше дефолт смотрел в `snens-release\snens`
  # (дерево 0.2.x), и запуск без аргументов собирал старую версию молча.
  [Parameter(Mandatory = $true)][string]$Repo,
  [string]$Key  = (Join-Path $env:USERPROFILE "snens-updater.key"),
  [string]$ClientDist = (Join-Path $env:USERPROFILE "snens-web-dist"),
  [string]$BinDir = "",
  [switch]$NoBuild
)

$log = Join-Path $Repo "tauri-signed.log"

# BinDir не задан — берём target-каталог самого репозитория, а не чужой путь.
if ([string]::IsNullOrWhiteSpace($BinDir)) {
  $BinDir = Join-Path $Repo "target\release"
}
if (-not (Test-Path (Join-Path $Repo "desktop\tauri.conf.json"))) {
  Write-Output "EXIT=1 (Repo=$Repo — это не корень репозитория snens)"
  exit 1
}
Write-Output ("repo: " + $Repo)

Write-Output "=== 1/3 сборка ядра и тулов"
if (-not $NoBuild) {
  Push-Location $Repo
  & cargo build --release --workspace *> (Join-Path $Repo "cargo-build.log")
  $code = $LASTEXITCODE
  Pop-Location
  if ($code -ne 0) {
    Get-Content (Join-Path $Repo "cargo-build.log") -Tail 20
    Write-Output "EXIT=$code (cargo build)"
    exit $code
  }
}

Write-Output "=== 2/3 раскладка (ядро, тулы, клиент)"
# Стейджа ОБЯЗАН идти до сборки окна: его build.rs проверяет, что
# `binaries/snens-<triple>.exe` есть, и падает иначе.
$sh = "C:\Program Files\Git\bin\sh.exe"
if (-not (Test-Path $sh)) { $sh = "C:\Program Files\Git\usr\bin\sh.exe" }
# После подмены пути проверка обязательна: без Git for Windows дальнейший вызов
# падал бы невнятно (или, хуже, `sh` из PATH оказался бы чужим).
if (-not (Test-Path $sh)) { Write-Output "EXIT=1 (нет sh.exe: поставь Git for Windows)"; exit 1 }
$env:SNENS_BIN_DIR = $BinDir
$env:SNENS_WEB_DIST = $ClientDist
& $sh (Join-Path $Repo "scripts\desktop-stage.sh") 2>&1 | ForEach-Object { Write-Output $_ }
if ($LASTEXITCODE -ne 0) { Write-Output "EXIT=$LASTEXITCODE (stage)"; exit $LASTEXITCODE }

# Проверка, что в бандл уехал СВЕЖИЙ клиент: дашборд и переключатель ядра
# появились в нём позже первой раскладки, и их отсутствие — та самая ошибка,
# которую ловим здесь, а не на машине человека.
$assetsDir = Join-Path $Repo "desktop\resources\web\assets"
if (-not (Test-Path $assetsDir)) {
  Write-Output "EXIT=1 (нет $assetsDir — стейдж не разложил клиент)"
  exit 1
}
# Ищем пусковые строки во ВСЕХ js-файлах бандла, а не в самом свежем: vite может
# разрезать код на несколько чанков, и нужный маркер оказался бы не в последнем
# по времени файле — проверка ложно ругалась бы «клиент устарел».
$jsFiles = Get-ChildItem $assetsDir -Filter "*.js"
if (-not $jsFiles) {
  Write-Output "EXIT=1 (в $assetsDir нет js — клиент собран не туда)"
  exit 1
}
$all = ($jsFiles | ForEach-Object { Get-Content $_.FullName -Raw }) -join "`n"
# Маркеры свежести — то, что живёт в JS, а не в CSS: класс вида `dash__card`
# переехал в 30-shell.css нового дизайна, и проверка врала «клиент устарел».
foreach ($needle in @("/v1/tickets", "snens-rail-mini")) {
  if ($all -notmatch [regex]::Escape($needle)) {
    Write-Output "EXIT=1 (клиент устарел: нет '$needle' ни в одном js из $assetsDir)"
    exit 1
  }
}
Write-Output "клиент свежий: $($jsFiles.Count) файл(ов), маркеры на месте"

Write-Output "=== 3/3 бандл с подписью"
# Ключ проверяем ДО запуска: если файла нет, `Get-Content` даёт non-terminating
# ошибку и переменная становится $null — tauri уходит в интерактивный запрос
# пароля, то есть в ровно тот вечный висяк, ради которого скрипт и написан.
if (-not (Test-Path -LiteralPath $Key)) {
  Write-Output "EXIT=1 (нет ключа подписи: $Key)"
  exit 1
}
Set-Location (Join-Path $Repo "desktop")
# Если `npx` упадёт терминативно, $code останется 1 — провал, а не «успех».
$code = 1
try {
  $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -LiteralPath $Key -Raw)
  if ([string]::IsNullOrWhiteSpace($env:TAURI_SIGNING_PRIVATE_KEY)) {
    Write-Output "EXIT=1 (ключ подписи пуст: $Key)"
    exit 1
  }
  # Ключ без пароля — и это значит «переменной НЕТ», а не «пустая строка».
  # tauri читает её через `env::var`: пустое значение — это Ok(""), и тогда он
  # идёт расшифровывать ключ и падает на «incorrect updater private key
  # password». Когда переменной нет — пишет «No updater private key password
  # provided, assuming an empty password» и подписывает. Так собирались 0.2.2 и
  # 0.3.0; на пустой строке сборка стоит.
  Remove-Item Env:\TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
  # `npm ci` обязателен: он ставит CLI ровно из закоммиченного
  # package-lock.json. Голый `npx` скачал бы тот 2.x, который редит реестр
  # сегодня, — а этим CLI подписывается инсталлятор.
  & npm ci --no-audit --no-fund *> $log
  if ($LASTEXITCODE -ne 0) {
    Get-Content $log -Tail 10
    Write-Output "EXIT=$LASTEXITCODE (npm ci)"
    exit $LASTEXITCODE
  }
  & npx tauri build *> $log
  $code = $LASTEXITCODE
} finally {
  # Приватный ключ не должен остаться в окружении сессии (и уехать в дочерние
  # процессы, которые запустят позже).
  Remove-Item Env:\TAURI_SIGNING_PRIVATE_KEY -ErrorAction SilentlyContinue
  Remove-Item Env:\TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
}
Get-Content $log -Tail 10
if ($code -ne 0) {
  Write-Output "EXIT=$code (tauri build)"
  exit $code
}
Write-Output "EXIT=0"
