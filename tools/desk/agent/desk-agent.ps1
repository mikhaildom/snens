# snens desk agent: управление окнами Windows через UI Automation.
#
# Живёт в интерактивной сессии пользователя (запуск через `schtasks /IT`), слушает
# 127.0.0.1:<порт> и говорит по одной JSON-строке на запрос — по одной JSON-строке
# в ответ. Клиент (`desk`) приходит через ssh-туннель.
#
# Почему постоянный процесс, а не «запустить скрипт на каждый вызов»: из ssh мы
# попадаем в сессию 0, где UIA видит НОЛЬ окон (проверено). Плюс запуск через
# schtasks стоит ~900 мс, а обход дерева — 43 мс: держать процесс дешевле.
param(
  [int]$Port = 9227,
  [string]$Token = ''
)

Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
Add-Type -Namespace SnensW -Name U -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int c);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
[DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
'@

$AE = [System.Windows.Automation.AutomationElement]
$TS = [System.Windows.Automation.TreeScope]
$CT = [System.Windows.Automation.ControlType]

# ---------------------------------------------------------------- утилиты

function Get-Root { $AE::RootElement }

Add-Type -Namespace SnensN -Name W -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr p);
[DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern int GetClassName(IntPtr h, System.Text.StringBuilder s, int n);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
[DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint f, uint x, uint y, uint d, IntPtr e);
public struct RECT { public int L,T,R,B; }
public delegate bool EnumProc(IntPtr h, IntPtr p);
'@

# Окна, которых нет в UIA-дереве как Window: системные диалоги (#32770) и прочее.
function Find-NativeWindows {
  $out = @()
  $cb = [SnensN.W+EnumProc]{
    param($h, $p)
    if (-not [SnensN.W]::IsWindowVisible($h)) { return $true }
    $t = New-Object System.Text.StringBuilder 512
    [void][SnensN.W]::GetWindowText($h, $t, 512)
    if ($t.Length -eq 0) { return $true }
    $c = New-Object System.Text.StringBuilder 256
    [void][SnensN.W]::GetClassName($h, $c, 256)
    $r = New-Object SnensN.W+RECT
    [void][SnensN.W]::GetWindowRect($h, [ref]$r)
    $script:__native += [pscustomobject]@{
      handle = $h.ToInt64(); name = $t.ToString(); class = $c.ToString(); pid = 0
      x = (Clamp-Int $r.L); y = (Clamp-Int $r.T)
      w = (Clamp-Int ($r.R - $r.L)); h = (Clamp-Int ($r.B - $r.T))
    }
    return $true
  }
  $script:__native = @()
  [void][SnensN.W]::EnumWindows($cb, [IntPtr]::Zero)
  return $script:__native
}

function Find-NativeWindow([string]$sel) {
  foreach ($w in (Find-NativeWindows)) {
    if ($w.name -and $w.name.ToLower().Contains($sel.ToLower())) { return $w }
  }
  return $null
}

# UIA отдаёт "Infinity"/"NaN" у свёрнутых и служебных окон — [int] на них падает.
function Clamp-Int([double]$v) {
  if ([double]::IsNaN($v) -or [double]::IsInfinity($v)) { return 0 }
  if ($v -gt 32000) { return 32000 }
  if ($v -lt -32000) { return -32000 }
  return [int]$v
}

function Find-Windows {
  $cond = New-Object System.Windows.Automation.PropertyCondition($AE::ControlTypeProperty, $CT::Window)
  $kids = (Get-Root).FindAll($TS::Children, $cond)
  $out = @()
  foreach ($w in $kids) {
    $r = $w.Current.BoundingRectangle
    $out += [pscustomobject]@{
      handle = $w.Current.NativeWindowHandle
      name   = $w.Current.Name
      class  = $w.Current.ClassName
      pid    = $w.Current.ProcessId
      # Рамка бывает «бесконечной» (у свёрнутых и служебных окон): приводим к
      # безопасному диапазону явной проверкой, иначе [int] падает на "Infinity".
      x = (Clamp-Int $r.X); y = (Clamp-Int $r.Y)
      w = (Clamp-Int $r.Width); h = (Clamp-Int $r.Height)
    }
  }
  return $out
}

# Окно по индексу из `apps` или по подстроке заголовка (регистр не важен).
function Resolve-Window([string]$sel) {
  $wins = Find-Windows
  # Системные диалоги (Save As, Open) — класс #32770: UIA отдаёт их как Window,
  # но без контролов внутри. Ищем по заголовку среди всех видимых окон сессии.
  if ($sel -and -not ($wins | Where-Object { $_.name -and $_.name.ToLower().Contains($sel.ToLower()) })) {
    $dlg = Find-NativeWindow $sel
    if ($dlg) { return $dlg }
  }
  if (-not $sel) {
    # по умолчанию — окно в фокусе
    $fg = [SnensW.U]::GetForegroundWindow()
    foreach ($w in $wins) { if ($w.handle -eq $fg.ToInt64()) { return $w } }
    if ($wins.Count -gt 0) { return $wins[0] }
    return $null
  }
  if ($sel -match '^\d+$') {
    $i = [int]$sel
    if ($i -ge 0 -and $i -lt $wins.Count) { return $wins[$i] }
    return $null
  }
  foreach ($w in $wins) { if ($w.name -and $w.name.ToLower().Contains($sel.ToLower())) { return $w } }
  return $null
}

function Get-WindowElement($win) {
  $cond = New-Object System.Windows.Automation.PropertyCondition($AE::NativeWindowHandleProperty, [int]$win.handle)
  return (Get-Root).FindFirst($TS::Children, $cond)
}

# Обход дерева в ширину: возвращает элементы с путём (индексы детей от окна).
# Путь — это адрес (`e12` в выводе), по нему потом находим элемент заново:
# AutomationElement нельзя сохранить между запросами, а путь стабилен.
function Walk-Tree($rootEl, [int]$MaxNodes = 4000, $MaxDepth = 0) {
  $result = New-Object System.Collections.ArrayList
  $queue = New-Object System.Collections.Queue
  $queue.Enqueue(@{ el = $rootEl; path = ''; depth = 0 })
  $n = 0
  while ($queue.Count -gt 0 -and $n -lt $MaxNodes) {
    $item = $queue.Dequeue()
    $n++
    [void]$result.Add($item)
    # --full: MaxDepth = 0 — без ограничения; иначе не глубже заданного.
    if ($MaxDepth -gt 0 -and $item.depth -ge $MaxDepth) { continue }
    $kids = $item.el.FindAll($TS::Children, [System.Windows.Automation.Condition]::TrueCondition)
    $i = 0
    foreach ($k in $kids) {
      $p = if ($item.path -eq '') { "$i" } else { "$($item.path)/$i" }
      $queue.Enqueue(@{ el = $k; path = $p; depth = $item.depth + 1 })
      $i++
    }
  }
  return $result
}

# Поиск элемента по имени или AutomationId внутри окна: нужен, чтобы действовать
# без предварительного `snap` (полное дерево на каждый шаг — дорого).
function Find-BySelector($winEl, [string]$name, [string]$aid) {
  if ($aid) {
    $c = New-Object System.Windows.Automation.PropertyCondition($AE::AutomationIdProperty, $aid)
    $e = $winEl.FindFirst($TS::Descendants, $c)
    if ($e) { return $e }
  }
  if ($name) {
    $c = New-Object System.Windows.Automation.PropertyCondition($AE::NameProperty, $name)
    $e = $winEl.FindFirst($TS::Descendants, $c)
    if ($e) { return $e }
  }
  return $null
}

function Resolve-ByPath($winEl, [string]$path) {
  if ($path -eq '' -or $path -eq $null) { return $winEl }
  $cur = $winEl
  foreach ($seg in $path.Split('/')) {
    $idx = [int]$seg
    $kids = $cur.FindAll($TS::Children, [System.Windows.Automation.Condition]::TrueCondition)
    if ($idx -lt 0 -or $idx -ge $kids.Count) { return $null }
    $cur = $kids[$idx]
  }
  return $cur
}

# Интерактивные типы — те, по которым можно действовать.
$script:Interactive = @('Button','Edit','Hyperlink','CheckBox','ComboBox','ListItem',
                        'MenuItem','TabItem','RadioButton','Slider','Spinner','TreeItem',
                        'Text','Document','SplitButton','ToolBar')

function Type-Name($el) {
  return $el.Current.ControlType.ProgrammaticName -replace '^ControlType\.', ''
}

function Is-Actionable($el) {
  $t = Type-Name $el
  if ($script:Interactive -notcontains $t) { return $false }
  # Text без имени бесполезен, а пустых Pane/Groups в дереве много
  if ($t -eq 'Text' -and -not $el.Current.Name) { return $false }
  return $true
}

function El-Line($el, [string]$addr, [int]$depth) {
  $t = Type-Name $el
  $name = $el.Current.Name
  $aid = $el.Current.AutomationId
  $val = ''
  try {
    $vp = $el.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
    $val = $vp.Current.Value
  } catch { }
  $state = @()
  if (-not $el.Current.IsEnabled) { $state += 'disabled' }
  if ($el.Current.IsOffscreen) { $state += 'offscreen' }
  if ($el.Current.HasKeyboardFocus) { $state += 'focused' }
  try {
    $tp = $el.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
    $state += $tp.Current.ToggleState  # On/Off/Indeterminate
  } catch { }

  $s = ('  ' * $depth) + $t
  if ($addr) { $s += " [$addr]" }
  if ($name) { $s += ' "' + ($name -replace '\s+', ' ') + '"' }
  if ($aid) { $s += " #$aid" }
  if ($val) { $s += ' = "' + ($val -replace '\s+', ' ') + '"' }
  if ($state.Count -gt 0) { $s += ' (' + ($state -join ',') + ')' }
  return $s
}

# ---------------------------------------------------------------- команды

function Cmd-Apps {
  $wins = Find-Windows
  $out = @()
  $i = 0
  foreach ($w in $wins) {
    $out += [pscustomobject]@{ i = $i; name = $w.name; class = $w.class; pid = $w.pid
                               x = $w.x; y = $w.y; w = $w.w; h = $w.h }
    $i++
  }
  return @{ ok = $true; windows = $out }
}

function Cmd-Snap($req) {
  $win = Resolve-Window $req.window
  if (-not $win) { return @{ ok = $false; error = "окно не найдено: $($req.window)" } }
  $winEl = Get-WindowElement $win
  if (-not $winEl) { return @{ ok = $false; error = "окно без UIA-элемента: $($win.name)" } }

  $sw = [System.Diagnostics.Stopwatch]::StartNew()
  # --full: без ограничения глубины (и с большим потолком узлов).
  $maxDepth = if ($req.full) { 0 } else { 24 }
  $maxNodes = if ($req.full) { 20000 } else { 4000 }
  $all = Walk-Tree $winEl $maxNodes $maxDepth
  $lines = New-Object System.Collections.ArrayList
  $refs = New-Object System.Collections.ArrayList
  $counter = 0
  foreach ($item in $all) {
    if (-not (Is-Actionable $item.el)) { continue }
    $counter++
    $addr = "e$counter"
    [void]$lines.Add((El-Line $item.el $addr $item.depth))
    [void]$refs.Add([pscustomobject]@{ addr = $addr; path = $item.path
                                       type = (Type-Name $item.el); name = $item.el.Current.Name
                                       aid = $item.el.Current.AutomationId })
  }
  return @{
    ok      = $true
    window  = $win.name
    pid     = $win.pid
    total   = $all.Count
    kept    = $refs.Count
    ms      = $sw.ElapsedMilliseconds
    tree    = ($lines -join "`n")
    refs    = $refs
  }
}

function Invoke-OnElement($el, [string]$kind, $arg) {
  switch ($kind) {
    'uia_click_at' {
      # Клик по центру элемента, найденного по AutomationId — обход отсутствия
      # нормального UIA-дерева у системных диалогов.
      return 'noop'
    }
    'click_sel' {
      # Селектор: {name:"Add to cart"} или {aid:"login-button"}. Так цепочка
      # «клик → ждать → клик» не требует снапшота между шагами.
      $sel = Find-BySelector $script:__winEl $arg.name $arg.aid
      if (-not $sel) { throw ("не нашёл элемент: name='$($arg.name)' aid='$($arg.aid)'") }
      return (Invoke-OnElement $sel 'click' $null)
    }
    'set_sel' {
      $sel = Find-BySelector $script:__winEl $arg.name $arg.aid
      if (-not $sel) { throw ("не нашёл поле: name='$($arg.name)' aid='$($arg.aid)'") }
      return (Invoke-OnElement $sel 'set' $arg.text)
    }
    'click_at' {
      # Клик по координатам внутри окна (диалоги, canvas, всё без UIA-узла).
      [void][SnensW.U]::SetCursorPos([int]$arg[0], [int]$arg[1])
      [SnensW.U]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
      [SnensW.U]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
      return ("mouse:" + $arg[0] + "," + $arg[1])
    }
    'click' {
      # Invoke — если есть (кнопки), иначе клик по центру (иконки, canvas)
      try {
        $p = $el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $p.Invoke(); return 'invoke'
      } catch { }
      try {
        $p = $el.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern)
        $p.Select(); return 'select'
      } catch { }
      $r = $el.Current.BoundingRectangle
      $x = [int]($r.X + $r.Width / 2); $y = [int]($r.Y + $r.Height / 2)
      [void][SnensW.U]::SetCursorPos($x, $y)
      [SnensW.U]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
      [SnensW.U]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
      return "mouse:$x,$y"
    }
    'set' {
      $p = $el.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern)
      $p.SetValue([string]$arg); return 'setvalue'
    }
    'toggle' {
      $p = $el.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
      $p.Toggle(); return 'toggle'
    }
    'focus' {
      try { $el.SetFocus(); return 'focus' } catch { }
      $r = $el.Current.BoundingRectangle
      [void][SnensW.U]::SetCursorPos([int]($r.X + $r.Width / 2), [int]($r.Y + $r.Height / 2))
      try {
        $p = $el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern); $p.Invoke()
      } catch { }
      return 'focus-click'
    }
    default { throw "неизвестное действие: $kind" }
  }
}

function Cmd-Act($req) {
  $win = Resolve-Window $req.window
  if (-not $win) { return @{ ok = $false; error = "окно не найдено: $($req.window)" } }
  $winEl = Get-WindowElement $win
  if ($req.focus_window) {
    # handle — Int64 из EnumWindows; [int] падал на значениях выше Int32.MaxValue.
    [void][SnensW.U]::ShowWindow([IntPtr]$win.handle, 9)
    [void][SnensW.U]::BringWindowToTop([IntPtr]$win.handle)
    [void][SnensW.U]::SetForegroundWindow([IntPtr]$win.handle)
  }
  $script:__winEl = $winEl
  $done = @()
  if (-not $winEl) {
    # Системный диалог (#32770) без UIA-начинки: работаем только координатами,
    # считая их от левого верхнего угла окна.
    foreach ($a in $req.actions) {
      $kind = $null
      foreach ($k in @('click_at','keys','wait')) { if ($a.PSObject.Properties.Name -contains $k) { $kind = $k; break } }
      # Значение действия раньше вообще не присваивалось: wait получал $null,
      # wait_sel/click_sel/set_sel были недостижимы.
      $arg = if ($kind) { $a.$kind } else { $null }
      if ($kind -eq 'wait_sel' -or $kind -eq 'click_sel' -or $kind -eq 'set_sel') {
        # В окне без UIA элементы не находятся: честный отказ, а не молчаливый пропуск.
        $done += "ошибка: $kind недоступен в окне без UIA (есть click_at/keys/wait)"
        continue
      }
      if ($kind -eq 'wait') {
        # Пауза: после клика страница/диалог ещё не готовы.
        $ms = [int]$arg
        if ($ms -le 0) { $ms = 500 }
        Start-Sleep -Milliseconds $ms
        $done += "wait $ms мс"
        continue
      }
      if ($kind -eq 'wait_sel') {
        # Ждать появления элемента: условие готовности вместо слепой паузы.
        $limit = [int]($arg.timeout_ms)
        if ($limit -le 0) { $limit = 8000 }
        $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
        $found = $null
        while ($sw2.ElapsedMilliseconds -lt $limit) {
          $found = Find-BySelector $script:__winEl $arg.name $arg.aid
          if ($found) { break }
          Start-Sleep -Milliseconds 200
        }
        if (-not $found) { $done += "wait_sel: не дождался name='$($arg.name)' aid='$($arg.aid)' за $limit мс" }
        else { $done += "wait_sel: появился за $($sw2.ElapsedMilliseconds) мс" }
        continue
      }
      if ($kind -eq 'click_sel' -or $kind -eq 'set_sel') {
        try { $how = Invoke-OnElement $null $kind $arg } catch { $done += "ошибка $kind : " + $_.Exception.Message; continue }
        $done += "$kind → $how"
        continue
      }
      if ($kind -eq 'click_at') {
        $ax = [int]$win.x + [int]$a.click_at[0]
        $ay = [int]$win.y + [int]$a.click_at[1]
        [void][SnensW.U]::SetCursorPos($ax, $ay)
        [SnensW.U]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)
        [SnensW.U]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)
        $done += "click_at(окно) $ax,$ay"
      } elseif ($kind -eq 'keys') {
        $ws = New-Object -ComObject WScript.Shell
        $ws.SendKeys([string]$a.keys)
        $done += "keys: $($a.keys)"
      } else {
        $done += "пропуск: в окне без UIA доступны click_at/keys"
      }
    }
    return @{ ok = $true; results = $done; window = $win.name }
  }
  foreach ($a in $req.actions) {
    # Действие — известное имя поля, а не «первое, что не ref»: порядок ключей в
    # JSON не гарантирован, и click принимался за пустое действие.
    $kind = $null
    foreach ($k in @('click','click_at','set','type','keys','toggle','focus','click_sel','set_sel','wait','wait_sel')) {
      if ($a.PSObject.Properties.Name -contains $k) { $kind = $k; break }
    }
    if (-not $kind) { $done += "пропуск: неизвестное действие (click/set/type/keys/toggle/focus)"; continue }
    $arg = $a.$kind
    try {
      if ($kind -eq 'keys') {
        # Клавиши: через WScript.Shell.SendKeys (работает в сессии пользователя)
        $ws = New-Object -ComObject WScript.Shell
        $ws.SendKeys([string]$arg)
        $done += "keys: $arg"
        continue
      }
      if ($kind -eq 'type') {
        $ws = New-Object -ComObject WScript.Shell
        $ws.SendKeys([string]$arg)
        $done += "type: " + ([string]$arg).Length + " символов"
        continue
      }
      # Адрес: либо в самом значении действия ({"click":"e39"}), либо отдельным
      # полем ref ({"ref":"e39","click":true}) — поддержаны обе формы.
      $addr = $null
      if ($a.ref) { $addr = [string]$a.ref }
      elseif ($arg -is [string] -and $arg -match '^e\d+$') { $addr = $arg }
      elseif ($arg -is [pscustomobject] -and $arg.ref) { $addr = [string]$arg.ref }
      elseif ($arg -is [System.Management.Automation.PSCustomObject] -and $arg.ref) { $addr = [string]$arg.ref }
      if ($kind -eq 'wait') {
        # Пауза: после клика страница/диалог ещё не готовы.
        $ms = [int]$arg
        if ($ms -le 0) { $ms = 500 }
        Start-Sleep -Milliseconds $ms
        $done += "wait $ms мс"
        continue
      }
      if ($kind -eq 'wait_sel') {
        # Ждать появления элемента: условие готовности вместо слепой паузы.
        $limit = [int]($arg.timeout_ms)
        if ($limit -le 0) { $limit = 8000 }
        $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
        $found = $null
        while ($sw2.ElapsedMilliseconds -lt $limit) {
          $found = Find-BySelector $script:__winEl $arg.name $arg.aid
          if ($found) { break }
          Start-Sleep -Milliseconds 200
        }
        if (-not $found) { $done += "wait_sel: не дождался name='$($arg.name)' aid='$($arg.aid)' за $limit мс" }
        else { $done += "wait_sel: появился за $($sw2.ElapsedMilliseconds) мс" }
        continue
      }
      if ($kind -eq 'click_sel' -or $kind -eq 'set_sel') {
        try { $how = Invoke-OnElement $null $kind $arg } catch { $done += "ошибка $kind : " + $_.Exception.Message; continue }
        $done += "$kind → $how"
        continue
      }
      if ($kind -eq 'click_at') {
        # Координаты — абсолютные экранные; клик мимо UIA.
        $how = Invoke-OnElement $null 'click_at' $arg
        $done += "click_at $($arg[0]),$($arg[1]) → $how"
        continue
      }
      if (-not $addr) { $done += "ошибка: в действии $kind нет адреса eN"; continue }
      $path = $null
      if ($addr -match '^e(\d+)$') {
        # Адрес — номер из последнего snap; путь приходит в req.refs.
        $idx = [int]($addr -replace '^e', '') - 1
        if ($req.refs -and $idx -lt $req.refs.Count) { $path = $req.refs[$idx].path }
      }
      if (-not $path) { $done += "ошибка: адрес $addr неизвестен (сделай snap)"; continue }
      $el = Resolve-ByPath $winEl $path
      if (-not $el) { $done += "ошибка: элемент $addr пропал (страница изменилась?)"; continue }
      # Значение действия: строка ({"set":"e1"} — но тогда нет текста) либо
      # объект {"ref":"e13","text":"..."} — текст берём из text.
      $value = $arg
      if ($arg -is [string]) { $value = '' }
      elseif ($null -ne $arg.text) { $value = $arg.text }
      elseif ($kind -eq 'set') { $done += "ошибка: в действии set нет text"; continue }
      $how = Invoke-OnElement $el $kind $value
      $done += "$kind $addr → $how"
    } catch {
      $done += "ошибка $kind $($a.ref): " + $_.Exception.Message
    }
  }
  return @{ ok = $true; results = $done; window = $win.name }
}

function Cmd-Shot($req) {
  Add-Type -AssemblyName System.Drawing
  $win = Resolve-Window $req.window
  if (-not $win) { return @{ ok = $false; error = "окно не найдено: $($req.window)" } }
  if ($req.focus_window) {
    [void][SnensW.U]::ShowWindow([IntPtr][int]$win.handle, 9)
    [void][SnensW.U]::SetForegroundWindow([IntPtr][int]$win.handle)
    Start-Sleep -Milliseconds 300
  }
  $rect = New-Object System.Drawing.Rectangle $win.x, $win.y, $win.w, $win.h
  $bmp = New-Object System.Drawing.Bitmap $rect.Width, $rect.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($rect.Location, [System.Drawing.Point]::Empty, $rect.Size)
  $ms = New-Object System.IO.MemoryStream
  $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
  $g.Dispose(); $bmp.Dispose()
  $b64 = [Convert]::ToBase64String($ms.ToArray())
  $ms.Dispose()
  return @{ ok = $true; png_base64 = $b64; window = $win.name; w = $win.w; h = $win.h }
}

function Cmd-Focus($req) {
  $win = Resolve-Window $req.window
  if (-not $win) { return @{ ok = $false; error = "окно не найдено: $($req.window)" } }
  [void][SnensW.U]::ShowWindow([IntPtr][int]$win.handle, 9)
  [void][SnensW.U]::BringWindowToTop([IntPtr][int]$win.handle)
  [void][SnensW.U]::SetForegroundWindow([IntPtr][int]$win.handle)
  return @{ ok = $true; window = $win.name }
}

# ---------------------------------------------------------------- сервер

function Write-Reply($stream, $obj) {
  # Глубина 6 и без форматирования: дерево бывает большим, но это одна строка.
  $json = $obj | ConvertTo-Json -Depth 6 -Compress
  $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
  $stream.Write($bytes, 0, $bytes.Length)
  $stream.Flush()
}

$logFile = Join-Path $env:USERPROFILE 'snens-desk.log'
function Log([string]$msg) {
  try { ("[{0}] {1}" -f (Get-Date -Format 'HH:mm:ss'), $msg) | Add-Content -Encoding UTF8 $logFile } catch { }
}
Log "start port=$Port session=$((Get-Process -Id $PID).SessionId) pid=$PID"
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, $Port)
try { $listener.Start() } catch { Log ("listen failed: " + $_.Exception.Message); throw }
Log "listening"
$ready = Join-Path $env:USERPROFILE 'snens-desk.out'
"ready port=$Port session=$((Get-Process -Id $PID).SessionId) pid=$PID" | Set-Content -Encoding ASCII $ready

try {
while ($true) {
  $client = $listener.AcceptTcpClient()
  Log "client connected"
  $client.NoDelay = $true
  # Без таймаута один молчащий клиент (повисший туннель, упавший вызывающий)
  # блокировал сервер навсегда: он однопоточный.
  $client.ReceiveTimeout = 60000
  $stream = $client.GetStream()
  $bytes = New-Object System.Collections.Generic.List[byte]
  $buffer = New-Object byte[] 8192
  try {
    while ($true) {
      # Читаем байты и режем по \n сами: StreamReader.ReadLine на этой связке
      # (PowerShell + GetStream) блокируется и не отдаёт строку до закрытия
      # соединения — клиент видел пустой ответ и «EOF».
      $n = $stream.Read($buffer, 0, $buffer.Length)
      if ($n -le 0) { break }
      for ($i = 0; $i -lt $n; $i++) { $bytes.Add($buffer[$i]) }
      while ($true) {
        $nl = $bytes.IndexOf([byte]10)
        if ($nl -lt 0) { break }
        $lineBytes = $bytes.GetRange(0, $nl).ToArray()
        $bytes.RemoveRange(0, $nl + 1)
        $line = [System.Text.Encoding]::UTF8.GetString($lineBytes).Trim()
        if ($line -eq '') { continue }
        $req = $null
        try { $req = $line | ConvertFrom-Json } catch {
          Write-Reply $stream (@{ ok = $false; error = 'не JSON' })
          continue
        }
        # -cne: сравнение с учётом регистра, иначе энтропия токена меньше, чем
        # кажется. Пустой токен больше не «режим без пароля».
        if (-not $Token -or $req.token -isnot [string] -or $req.token -cne $Token) {
          Write-Reply $stream (@{ ok = $false; error = 'неверный токен' })
          continue
        }
        $resp = $null
        try {
          switch ($req.cmd) {
            'apps'  { $resp = Cmd-Apps }
            'snap'  { $resp = Cmd-Snap $req }
            'act'   { $resp = Cmd-Act $req }
            'shot'  { $resp = Cmd-Shot $req }
            'focus' { $resp = Cmd-Focus $req }
            'ping'  { $resp = @{ ok = $true; session = (Get-Process -Id $PID).SessionId; pid = $PID } }
            default { $resp = @{ ok = $false; error = "неизвестная команда: $($req.cmd)" } }
          }
        } catch {
          Log ("cmd " + $req.cmd + " failed: " + $_.Exception.Message)
          $resp = @{ ok = $false; error = $_.Exception.Message }
        }
        Write-Reply $stream $resp
      }
    }
  } catch {
    Log ("session error: " + $_.Exception.Message)
  } finally {
    $stream.Dispose(); $client.Close()
  }
}
} catch {
  Log ("FATAL: " + $_.Exception.Message + " | " + $_.InvocationInfo.PositionMessage)
  throw
}
