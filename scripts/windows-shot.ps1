# Открыть дашборд в запущенном окне snens и снять экран.
#
# Зачем: ssh-сессия неинтерактивна, скриншот и клики по окну из неё невозможны
# (WebView2 не создаёт окно, CopyFromScreen падает). Скрипт запускается
# планировщиком с /it — в живой сессии пользователя.
$out = "C:\Users\Svuvich\dash-state.txt"
$png = "C:\Users\Svuvich\dash.png"

$lines = New-Object System.Collections.ArrayList
[void]$lines.Add("interactive=" + [Environment]::UserInteractive)
foreach ($p in Get-Process | Where-Object { $_.ProcessName -like "snens*" }) {
  [void]$lines.Add("proc " + $p.ProcessName + " id=" + $p.Id + " session=" + $p.SessionId + " title='" + $p.MainWindowTitle + "'")
}

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
}

$lines | Set-Content -Encoding UTF8 $out
