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

# Глобальный тип PowerShell создаётся один раз на сессию: повторный Add-Type с
# тем же именем падает, если скрипт запускают дважды из одного хоста
# (например, из одной сессии планировщика).
if (-not ("C" -as [type])) {
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
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, Rr, B; }
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
}

function Report($tag) {
  $p = Get-Process snens-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $p) { [void]$log.Add("$tag : ПРОЦЕССА НЕТ"); return }
  $visible = @()
  foreach ($h in [C]::Wins([uint32]$p.Id)) {
    if ([C]::IsWindowVisible($h)) { $visible += [C]::Title($h) }
  }
  [void]$log.Add("$tag : pid=$($p.Id) видимых окон=$($visible.Count) [$($visible -join ', ')]")
}

# Главное окно процесса — самое большое видимое (раньше брали по точному
# заголовку «snens», а комментарий обещал «самое большое»: при смене заголовка
# harness просто перестал бы находить окно).
function MainWindow {
  $p = Get-Process snens-desktop -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $p) { return [IntPtr]::Zero }
  $best = [IntPtr]::Zero
  $bestArea = 0
  foreach ($h in [C]::Wins([uint32]$p.Id)) {
    if (-not [C]::IsWindowVisible($h)) { continue }
    $r = New-Object C+RECT
    if (-not [C]::GetWindowRect($h, [ref]$r)) { continue }
    $area = ($r.Rr - $r.L) * ($r.B - $r.T)
    if ($area -gt $bestArea) { $bestArea = $area; $best = $h }
  }
  return $best
}

# Ждать состояния, а не фиксированный сон: на загруженной машине 2 с мало, на
# быстрой — лишнее ожидание.
function Wait-Visible([int]$seconds) {
  $until = (Get-Date).AddSeconds($seconds)
  while ((Get-Date) -lt $until) {
    if ((MainWindow) -ne [IntPtr]::Zero) { return $true }
    Start-Sleep -Milliseconds 200
  }
  return ((MainWindow) -ne [IntPtr]::Zero)
}

function Wait-Hidden([int]$seconds) {
  $until = (Get-Date).AddSeconds($seconds)
  while ((Get-Date) -lt $until) {
    if ((MainWindow) -eq [IntPtr]::Zero) { return $true }
    Start-Sleep -Milliseconds 200
  }
  return ((MainWindow) -eq [IntPtr]::Zero)
}

Report "старт"

# 1) крестик: WM_CLOSE — то же, что кнопка окна
$h = MainWindow
if ($h -ne [IntPtr]::Zero) {
  [void][C]::SetForegroundWindow($h)
  [void][C]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)  # WM_CLOSE
  [void](Wait-Hidden 10)
}
Report "после первого крестика"

# 2) повторный запуск — второй экземпляр просит первый показать окно
$second = Start-Process -FilePath $Exe -PassThru -ErrorAction Stop
[void](Wait-Visible 15)
Report "после повторного запуска"

# 3) ещё один крестик — вот здесь окно раньше не закрывалось
$h = MainWindow
if ($h -ne [IntPtr]::Zero) {
  [void][C]::SetForegroundWindow($h)
  [void][C]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
  [void](Wait-Hidden 10)
}
Report "после второго крестика"

# 4) и ещё раз: показать, что цикл не сломался
$third = Start-Process -FilePath $Exe -PassThru -ErrorAction Stop
[void](Wait-Visible 15)
Report "после третьего запуска"
$h = MainWindow
if ($h -ne [IntPtr]::Zero) {
  [void][C]::SetForegroundWindow($h)
  [void][C]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
  [void](Wait-Hidden 10)
}
Report "после третьего крестика"

$log | Set-Content -Encoding UTF8 $out
