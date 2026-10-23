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
  [string]$Repo = "C:\Users\Svuvich\snens-release\snens",
  [string]$Key = "C:\Users\Svuvich\snens-updater.key",
  [string]$ClientDist = "C:\Users\Svuvich\snens-web-dist",
  [string]$BinDir = "C:\Users\Svuvich\snens-release\snens\target\release",
  [switch]$NoBuild
)

$log = Join-Path $Repo "tauri-signed.log"

if (-not $NoBuild) {
  Write-Output "=== 1/3 сборка ядра и тулов"
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
# sh из Git for Windows: desktop-stage.sh — обычный POSIX-скрипт.
$sh = "C:\Program Files\Git\bin\sh.exe"
if (-not (Test-Path $sh)) { $sh = "C:\Program Files\Git\usr\bin\sh.exe" }
$env:SNENS_BIN_DIR = $BinDir
$env:SNENS_WEB_DIST = $ClientDist
& $sh (Join-Path $Repo "scripts\desktop-stage.sh") 2>&1 | ForEach-Object { Write-Output $_ }
if ($LASTEXITCODE -ne 0) { Write-Output "EXIT=$LASTEXITCODE (stage)"; exit $LASTEXITCODE }

# Проверка, что в бандл уехал СВЕЖИЙ клиент: дашборд и переключатель ядра
# появились в нём позже первой раскладки, и их отсутствие — та самая ошибка,
# которую ловим здесь, а не на машине человека.
$js = Get-ChildItem (Join-Path $Repo "desktop\resources\web\assets") -Filter "*.js" |
      Sort-Object LastWriteTime -Descending | Select-Object -First 1
$src = Get-Content $js.FullName -Raw
foreach ($needle in @("/v1/tickets", "dash__card")) {
  if ($src -notmatch [regex]::Escape($needle)) {
    Write-Output "EXIT=1 (клиент устарел: нет '$needle' в $($js.Name))"
    exit 1
  }
}
Write-Output "клиент свежий: $($js.Name)"

Write-Output "=== 3/3 бандл с подписью"
Set-Location (Join-Path $Repo "desktop")
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $Key -Raw)
# Ключ без пароля; пустая строка — это «пароля нет», а не «не задан».
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
& npx tauri build *> $log
Write-Output "EXIT=$LASTEXITCODE"
Get-Content $log -Tail 10
