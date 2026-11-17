# Запустить окно snens с включённой отладкой WebView2.
#
# Зачем: проверить поведение интерфейса в РЕАЛЬНОМ окне (клик по кнопке,
# состояние страницы) можно только через CDP. WebView2, в отличие от Chrome,
# порт не открывает, пока не попросишь: `--remote-debugging-port` передаётся
# через WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS.
#
# Запускается планировщиком с /it — в ssh-сессии нет рабочего стола.
#
# Осторожно: порт CDP не аутентифицирован — любой местный процесс с доступом к
# 127.0.0.1:9333 управляет окном. Включать только на время отладки.
#
# Один экземпляр: если snens-desktop уже работает, новый процесс лишь попросит
# первый показать окно и выйдет — отладочного порта у существующего окна не
# появится. Поэтому старый экземпляр надо закрыть заранее, и это проверяем.
param(
  [string]$Exe = (Join-Path $env:LOCALAPPDATA 'snens\snens-desktop.exe'),
  [int]$Port = 9333
)

$running = Get-Process snens-desktop -ErrorAction SilentlyContinue
if ($running) {
  Write-Error "snens-desktop уже запущен (pid=$($running[0].Id)): закрой его — иначе отладочного порта не будет"
  exit 1
}
if (-not (Test-Path -LiteralPath $Exe)) {
  Write-Error "не найден $Exe"
  exit 1
}

$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port"
$proc = Start-Process -FilePath $Exe -PassThru -ErrorAction Stop
Write-Output "запущено: pid=$($proc.Id), CDP на 127.0.0.1:$Port"
