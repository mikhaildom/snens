# Открыть дашборд в окне snens и снять экран — в интерактивной сессии.
#
# Запускается планировщиком с /it: в ssh-сессии нет рабочего стола, ни клик,
# ни скриншот там невозможны ("handle is invalid", WebView2 не создаёт окно).
#
# Окно ищем по PID нашего процесса, а не по заголовку: у WebView2 верхнеуровневое
# окно принадлежит тому же процессу, но FindWindow по title его не находит
# (заголовок ставит Tauri, а классы у окна и у webview разные) — проверено.
$out = Join-Path $env:USERPROFILE "dash-click.txt"
$png = Join-Path $env:USERPROFILE "dash-open.png"
# Глобальный тип PowerShell создаётся один раз на сессию: повторный Add-Type с
# тем же именем падает, если скрипт запускают дважды из одного хоста
# (например, из одной сессии планировщика).
if (-not ("W" -as [type])) {
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class W {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, Rr, B; }
  public static List<IntPtr> ByPid(uint want) {
    var res = new List<IntPtr>();
    EnumWindows((h, l) => {
      uint pid; GetWindowThreadProcessId(h, out pid);
      if (pid == want && IsWindowVisible(h)) res.Add(h);
      return true;
    }, IntPtr.Zero);
    return res;
  }
}
"@
}

$lines = New-Object System.Collections.ArrayList
$proc = Get-Process snens-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $proc) { @("no snens-desktop process") | Set-Content -Encoding UTF8 $out; exit 1 }
[void]$lines.Add("pid=" + $proc.Id)

$wins = [W]::ByPid([uint32]$proc.Id)
[void]$lines.Add("windows=" + $wins.Count)
$target = [IntPtr]::Zero
$best = 0
foreach ($h in $wins) {
  $sb = New-Object System.Text.StringBuilder 512
  [void][W]::GetWindowText($h, $sb, 512)
  $t = $sb.ToString()
  $rr = New-Object W+RECT
  [void][W]::GetWindowRect($h, [ref]$rr)
  $area = ($rr.Rr - $rr.L) * ($rr.B - $rr.T)
  [void]$lines.Add("win '$t' area=$area")
  if ($area -gt $best) { $best = $area; $target = $h }
}
if ($target -eq [IntPtr]::Zero) { $lines | Set-Content -Encoding UTF8 $out; exit 1 }

[void][W]::ShowWindow($target, 9)   # SW_RESTORE
[void][W]::SetForegroundWindow($target)
Start-Sleep -Milliseconds 800

$r = New-Object W+RECT
[void][W]::GetWindowRect($target, [ref]$r)
[void]$lines.Add("rect=$($r.L),$($r.T),$($r.Rr),$($r.B)")

# Кнопка дашборда в шапке списка сессий: рельса ~248 px, строка «sessions N [▤][+]».
# Координаты МАГИЧЕСКИЕ: они верны только при 100% масштабировании экрана и на
# нынешней вёрстке шапки. При другом DPI или после правки клиента поправь их
# (или сними дашборд через CDP — tools/cdp — там координаты не нужны).
$scale = 1.0
try {
  Add-Type -AssemblyName System.Windows.Forms
  $scale = [System.Windows.Forms.SystemInformation]::PrimaryMonitorSize.Width /
           ([System.Windows.Forms.Screen]::PrimaryScreen.Bounds.Width)
} catch { $scale = 1.0 }
if ([math]::Abs($scale - 1.0) -gt 0.01) {
  # Смещение считается в DIP: при масштабе != 100% пиксельный клик уедет мимо.
  [void]$lines.Add("scale=$scale — координаты клика верны только при 100%, проверь результат глазами")
}
$x = $r.L + [int](232 * $scale)
$y = $r.T + [int](97 * $scale)

# Клик синтезируется глобально: если окно snens не оказалось поверх, клик уедет
# в чужой интерфейс. `SetForegroundWindow` отказывает регулярно (таймаут
# блокировки фокуса, другой уровень целостности) и возвращает false — проверяем
# фактическое состояние, а не намерение.
if ([W]::GetForegroundWindow() -ne $target) {
  [void]$lines.Add("target is not foreground, click aborted")
  $lines | Set-Content -Encoding UTF8 $out
  exit 1
}

$prev = New-Object W+POINT
$hadPrev = [W]::GetCursorPos([ref]$prev)
[void][W]::SetCursorPos($x, $y)
Start-Sleep -Milliseconds 250
[W]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
[W]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
# Курсор возвращаем на место: человек не должен искать свою мышку.
if ($hadPrev) { [void][W]::SetCursorPos($prev.X, $prev.Y) }
Start-Sleep -Milliseconds 2000

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
  if ($g) { $g.Dispose() }
  if ($bmp) { $bmp.Dispose() }
}
$lines | Set-Content -Encoding UTF8 $out
