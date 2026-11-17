# Снять состояние рабочего стола из ИНТЕРАКТИВНОЙ сессии Windows.
#
# Зачем так: ssh-сессия на этой машине неинтерактивна ([Environment]::UserInteractive
# = False, SessionId отличается от сессии explorer). В ней WebView2 не может
# создать окно ("Invalid window handle", HRESULT 0x80070578) и CopyFromScreen
# падает с "handle is invalid" — это ограничение среды, а не приложения.
# Поэтому скрипт запускается планировщиком с флагом /it в сессии пользователя.
#
# Пути — от профиля текущего пользователя, а не зашитые: на другом аккаунте
# `C:\Users\Svuvich` не существует.
param(
  [string]$Out = (Join-Path $env:USERPROFILE 'desk-state.txt'),
  [string]$Png = (Join-Path $env:USERPROFILE 'desk.png')
)
$out = $Out
$png = $Png

# Глобальный тип PowerShell создаётся один раз на сессию: повторный Add-Type с
# тем же именем падает, если скрипт запускают дважды из одного хоста
# (например, из одной сессии планировщика).
if (-not ("Win" -as [type])) {
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win {
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr l);
  public delegate bool EnumWindowsProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
}
"@
}

$lines = New-Object System.Collections.ArrayList
$cb = [Win+EnumWindowsProc]{
  param($h, $l)
  if ([Win]::IsWindowVisible($h)) {
    $sb = New-Object System.Text.StringBuilder 512
    [void][Win]::GetWindowText($h, $sb, 512)
    $t = $sb.ToString()
    if ($t) {
      # Не `$pid`: в PowerShell это автоматическая read-only переменная (PID
      # хоста), присвоение в неё падает — и все окна резолвились бы в сам хост.
      $procId = 0
      [void][Win]::GetWindowThreadProcessId($h, [ref]$procId)
      $pname = (Get-Process -Id $procId -ErrorAction SilentlyContinue).ProcessName
      [void]$lines.Add("$pname | $t")
    }
  }
  return $true
}
[void][Win]::EnumWindows($cb, [IntPtr]::Zero)

$state = @()
$state += "interactive=" + [Environment]::UserInteractive
$state += "session=" + (Get-Process -Id $PID).SessionId
foreach ($p in Get-Process | Where-Object { $_.ProcessName -like "snens*" }) {
  $state += ("proc " + $p.ProcessName + " id=" + $p.Id + " session=" + $p.SessionId + " title='" + $p.MainWindowTitle + "'")
}
$state += "--- visible windows ---"
$state += $lines
$state | Set-Content -Encoding UTF8 $out

try {
  Add-Type -AssemblyName System.Windows.Forms, System.Drawing
  $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $bmp = New-Object Drawing.Bitmap $b.Width, $b.Height
  $g = [Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
  $bmp.Save($png, [Drawing.Imaging.ImageFormat]::Png)
  Add-Content -Encoding UTF8 $out "screenshot=ok"
} catch {
  Add-Content -Encoding UTF8 $out ("screenshot=ERR " + $_.Exception.Message)
} finally {
  # Без Dispose на каждый прогон остаётся битмап размером с экран.
  if ($g) { $g.Dispose() }
  if ($bmp) { $bmp.Dispose() }
}
