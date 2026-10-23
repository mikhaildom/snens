# Запустить окно snens с включённой отладкой WebView2.
#
# Зачем: проверить поведение интерфейса в РЕАЛЬНОМ окне (клик по кнопке,
# состояние страницы) можно только через CDP. WebView2, в отличие от Chrome,
# порт не открывает, пока не попросишь: `--remote-debugging-port` передаётся
# через WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS.
#
# Запускается планировщиком с /it — в ssh-сессии нет рабочего стола.
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9333"
Start-Process -FilePath "C:\Users\Svuvich\AppData\Local\snens\snens-desktop.exe"
