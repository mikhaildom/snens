# Открыть дашборд в запущенном окне snens и снять экран.
#
# Зачем: ssh-сессия неинтерактивна, скриншот и клики по окну из неё невозможны
# (WebView2 не создаёт окно, CopyFromScreen падает). Скрипт запускается
# планировщиком с /it — в живой сессии пользователя.
#
# Пути — от профиля текущего пользователя, а не зашитые в скрипт: на другом
# аккаунте (`C:\Users\Svuvich` не существует) запись падала бы уже после
# побочных эффектов, да и ошибка была бы невнятной.
param(
  [string]$Out = (Join-Path $env:USERPROFILE 'dash-state.txt'),
  [string]$Png = (Join-Path $env:USERPROFILE 'dash.png')
)
$out = $Out
$png = $Png

$lines = New-Object System.Collections.ArrayList
[void]$lines.Add("interactive=" + [Environment]::UserInteractive)
foreach ($p in Get-Process | Where-Object { $_.ProcessName -like "snens*" }) {
  [void]$lines.Add("proc " + $p.ProcessName + " id=" + $p.Id + " session=" + $p.SessionId + " title='" + $p.MainWindowTitle + "'")
}

$bmp = $null
$g = $null
try {
  Add-Type -AssemblyName System.Windows.Forms, System.Drawing
  $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bmp = New-Object Drawing.Bitmap $b.Width, $b.Height
  $g = [Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
  $bmp.Save($png, [Drawing.Imaging.ImageFormat]::Png)
  [void]$lines.Add("screenshot=ok")
} catch {
  [void]$lines.Add("screenshot=ERR " + $_.Exception.Message)
} finally {
  # Без Dispose на каждый прогон остаётся битмап размером с экран.
  if ($g) { $g.Dispose() }
  if ($bmp) { $bmp.Dispose() }
}

# Итог важнее процесса: оформляем ошибку как ошибку, а не как «скрипт промолчал».
try {
  $lines | Set-Content -Encoding UTF8 -ErrorAction Stop $out
} catch {
  Write-Error "не записать $out : $($_.Exception.Message)"
  exit 1
}
