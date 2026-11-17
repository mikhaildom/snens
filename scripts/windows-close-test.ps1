# Воспроизвести сценарий с крестиком и записать результат.
#
# Шаги владельца: запустить → закрыть крестиком → запустить заново → закрыть
# крестиком. На этом втором закрытии окно не пряталось: показ окна вторым
# экземпляром шёл в обход Tauri, тот считал окно спрятанным, и `hide()`
# становился no-op.
#
# Проверяем не «кликнулось ли», а состояние: после крестика окна не видно
# (IsWindowVisible = false), при этом процесс жив (это «спрятать», не «выйти»).
#
# Пути — от профиля текущего пользователя: на другом аккаунте зашитые
# `C:\Users\Svuvich` не существуют, и harness логировал бы фальшивый результат.
param(
  [string]$Exe = (Join-Path $env:LOCALAPPDATA 'snens\snens-desktop.exe'),
  [string]$Out = (Join-Path $env:USERPROFILE 'close-test.txt')
)
$out = $Out
$log = New-Object System.Collections.ArrayList

# Стартовое предусловие: окно должно быть запущено человеком. Иначе весь
# сценарий отрабатывает вхолостую и отчёт выглядит успешным.
if (-not (Get-Process snens-desktop -ErrorAction SilentlyContinue)) {
  @("ПРЕДУСЛОВИЕ НЕ ВЫПОЛНЕНО: snens-desktop не запущен") | Set-Content -Encoding UTF8 $out
  Write-Error "snens-desktop не запущен: запусти приложение и повтори"
  exit 1
}
if (-not (Test-Path -LiteralPath $Exe)) {
  @("ПРЕДУСЛОВИЕ НЕ ВЫПОЛНЕНО: нет $Exe") | Set-Content -Encoding UTF8 $out
  Write-Error "не найден $Exe"
  exit 1
}

Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public class C {
  public delegate bool Proc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(Proc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  public static List<IntPtr> Wins(uint want) {
    var r = new List<IntPtr>();
    EnumWindows((h,l) => { uint p; GetWindowThreadProcessId(h,out p); if (p==want) r.Add(h); return true; }, IntPtr.Zero);
    return r;
  }
  public static string Title(IntPtr h) {
    var sb = new StringBuilder(512); GetWindowText(h, sb, 512); return sb.ToString();
  }
}
"@

function Report($tag) {
  $p = Get-Process snens-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $p) { [void]$log.Add("$tag : ПРОЦЕССА НЕТ"); return }
  $visible = @()
  foreach ($h in [C]::Wins([uint32]$p.Id)) {
    if ([C]::IsWindowVisible($h)) { $visible += [C]::Title($h) }
  }
  [void]$log.Add("$tag : pid=$($p.Id) видимых окон=$($visible.Count) [$($visible -join ', ')]")
}

# Главное окно процесса — самое большое видимое. По нему и жмём крестик.
function MainWindow {
  $p = Get-Process snens-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $p) { return [IntPtr]::Zero }
  foreach ($h in [C]::Wins([uint32]$p.Id)) {
    if ([C]::IsWindowVisible($h) -and [C]::Title($h) -eq "snens") { return $h }
  }
  return [IntPtr]::Zero
}

Report "старт"

# 1) крестик: WM_CLOSE — то же, что кнопка окна
$h = MainWindow
if ($h -ne [IntPtr]::Zero) {
  [void][C]::SetForegroundWindow($h)
  [void][C]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)  # WM_CLOSE
  Start-Sleep -Seconds 2
}
Report "после первого крестика"

# 2) повторный запуск — второй экземпляр просит первый показать окно
$second = Start-Process -FilePath $Exe -PassThru -ErrorAction Stop
Start-Sleep -Seconds 4
Report "после повторного запуска"

# 3) ещё один крестик — вот здесь окно раньше не закрывалось
$h = MainWindow
if ($h -ne [IntPtr]::Zero) {
  [void][C]::SetForegroundWindow($h)
  [void][C]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
  Start-Sleep -Seconds 2
}
Report "после второго крестика"

# 4) и ещё раз: показать, что цикл не сломался
$third = Start-Process -FilePath $Exe -PassThru -ErrorAction Stop
Start-Sleep -Seconds 4
Report "после третьего запуска"
$h = MainWindow
if ($h -ne [IntPtr]::Zero) {
  [void][C]::SetForegroundWindow($h)
  [void][C]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
  Start-Sleep -Seconds 2
}
Report "после третьего крестика"

$log | Set-Content -Encoding UTF8 $out
