# NeoSonia UI 驱动库（供测试脚本 dot-source 使用）
#
# 用法：
#   . "$PSScriptRoot\ui-drive.ps1"
#   $win = Get-NeoWindow
#   Invoke-NeoClickLogical 158 68      # 点击导航栏「设置」（逻辑坐标）
#   Save-NeoScreenshot "E:\NeoSonia\logs\ui\shot.png"
#
# 坐标约定：
# - 带 Logical 后缀的函数参数是“逻辑像素”（窗口内部布局坐标，按 96 DPI 计），
#   函数内部按窗口 DPI 缩放成物理像素；换显示器/改缩放不用改测试。
# - 不带 Logical 的是“物理像素”，相对窗口左上角；只在确实要精确到物理像素时用。
# - 本库在加载时调用 SetProcessDPIAware()，保证 GetWindowRect / 截图 / 鼠标坐标都是
#   真实物理像素，不会出现“125%/250% 缩放下点到隔壁按钮”的问题。
#
# 关于全局快捷键（重要限制，别浪费时间模拟）：
# - app 的 Windows 全局快捷键走键盘钩子，钩子会忽略带 LLKHF_INJECTED 的注入事件，
#   而 SendInput 发出的按键一定带这个标志，所以 Send-NeoHotkey 无法触发 NeoSonia
#   自己的 Alt+Space。
# - 要验证热键只能：a) 检查日志里钩子已安装（Test-NeoHotkeyHookInstalled）；
#   b) 用 --ask 模式让 agent 走 screenshot 工具验证截图链路；c) 人工按一次。
# - Send-NeoHotkey 仍然保留：对不筛注入事件的其它程序/系统组合键有用。
#
# 依赖：Windows PowerShell 5.1+，System.Drawing（.NET Framework 自带）。

if (-not ("NeoWin" -as [type])) {
    $neoWinCode = @"
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public class NeoWin {
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowW(string lpClassName, string lpWindowName);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint idAttach, uint idAttachTo, bool fAttach);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, IntPtr extra);
    [DllImport("user32.dll")] public static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);
    [DllImport("user32.dll")] public static extern uint MapVirtualKey(uint code, uint mapType);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();

    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L; public int T; public int R; public int B; }
    [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
    [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; }
    [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION u; }

    public const uint INPUT_MOUSE = 0;
    public const uint INPUT_KEYBOARD = 1;
    public const uint MOUSEEVENTF_MOVE = 0x0001;
    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const uint MOUSEEVENTF_LEFTUP = 0x0004;
    public const uint MOUSEEVENTF_WHEEL = 0x0800;
    public const uint KEYEVENTF_EXTENDEDKEY = 0x0001;
    public const uint KEYEVENTF_KEYUP = 0x0002;
    public const uint KEYEVENTF_SCANCODE = 0x0008;
    public const uint KEYEVENTF_UNICODE = 0x0004;

    public static void InitDpi() { try { SetProcessDPIAware(); } catch { } }

    public delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr hWnd, System.Text.StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);

    /// 找标题为 NeoSonia 的窗口。优先 FindWindowW；拿不到时按标题枚举兜底
    /// （真机上见过 FindWindow 偶尔返回 0、但窗口明明存在的情况）。
    public static IntPtr Find() {
        InitDpi();
        IntPtr h = FindWindowW(null, "NeoSonia");
        if (h != IntPtr.Zero) return h;
        IntPtr found = IntPtr.Zero;
        EnumWindows(delegate(IntPtr w, IntPtr l) {
            var sb = new System.Text.StringBuilder(256);
            GetWindowTextW(w, sb, 256);
            if (sb.ToString() == "NeoSonia" && IsWindowVisible(w)) { found = w; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static uint Dpi(IntPtr h) { if (h == IntPtr.Zero) return 96; uint d = GetDpiForWindow(h); return d == 0 ? 96 : d; }
    public static RECT Rect(IntPtr h) { RECT r; GetWindowRect(h, out r); return r; }

    /// 强制把窗口带到前台：先置顶，再 AttachThreadInput 到前台线程和窗口线程，最后 SetForegroundWindow。
    public static bool Focus(IntPtr h) {
        if (h == IntPtr.Zero) return false;
        InitDpi();
        SetWindowPos(h, new IntPtr(-1), 0, 0, 0, 0, 0x0001 | 0x0002);
        IntPtr fg = GetForegroundWindow();
        uint fgPid = 0, winPid = 0;
        uint fgTid = GetWindowThreadProcessId(fg, out fgPid);
        uint winTid = GetWindowThreadProcessId(h, out winPid);
        uint cur = GetCurrentThreadId();
        AttachThreadInput(cur, fgTid, true);
        AttachThreadInput(cur, winTid, true);
        BringWindowToTop(h);
        bool ok = SetForegroundWindow(h);
        AttachThreadInput(cur, fgTid, false);
        AttachThreadInput(cur, winTid, false);
        return ok;
    }

    public static void Move(int x, int y) { SetCursorPos(x, y); }

    /// 真机踩坑：fenestra 的滚轮要目标窗口先收到鼠标移动（hover）才认。
    /// 这里先移到目标点左侧 2px，再发一次相对 MOVE 事件，最后回到目标点。
    public static void Hover(int x, int y) {
        SetCursorPos(x - 2, y);
        System.Threading.Thread.Sleep(40);
        mouse_event(MOUSEEVENTF_MOVE, 1, 0, 0, IntPtr.Zero);
        System.Threading.Thread.Sleep(40);
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(80);
    }
    public static void Click(int x, int y) {
        SetCursorPos(x, y);
        System.Threading.Thread.Sleep(120);
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, IntPtr.Zero);
        System.Threading.Thread.Sleep(70);
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, IntPtr.Zero);
    }
    public static void Wheel(int notches) {
        // 正数 = 向下滚，负数 = 向上滚；一格 120。
        mouse_event(MOUSEEVENTF_WHEEL, 0, 0, unchecked((uint)(-120 * notches)), IntPtr.Zero);
    }

    /// SendInput 打 Unicode 文本：每个 UTF-16 码元一组（按下 wFlags=UNICODE，抬起再 +KEYUP）。
    public static void TypeText(string text) {
        if (text == null || text.Length == 0) return;
        for (int i = 0; i < text.Length; i++) {
            INPUT down = new INPUT();
            down.type = INPUT_KEYBOARD;
            down.u.ki.wVk = 0;
            down.u.ki.wScan = (ushort)text[i];
            down.u.ki.dwFlags = KEYEVENTF_UNICODE;
            INPUT up = new INPUT();
            up.type = INPUT_KEYBOARD;
            up.u.ki.wVk = 0;
            up.u.ki.wScan = (ushort)text[i];
            up.u.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
            INPUT[] pair = new INPUT[] { down, up };
            SendInput(2, pair, Marshal.SizeOf(typeof(INPUT)));
            System.Threading.Thread.Sleep(12);
        }
    }

    /// 按键（扫描码方式，带扩展键标记）。热键请用 SendHotkey。
    public static void Key(ushort vk) {
        INPUT down = new INPUT();
        down.type = INPUT_KEYBOARD;
        down.u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
        down.u.ki.dwFlags = KEYEVENTF_SCANCODE | (IsExtended(vk) ? KEYEVENTF_EXTENDEDKEY : 0);
        INPUT[] one = new INPUT[] { down };
        SendInput(1, one, Marshal.SizeOf(typeof(INPUT)));
        System.Threading.Thread.Sleep(30);
        INPUT up = new INPUT();
        up.type = INPUT_KEYBOARD;
        up.u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
        up.u.ki.dwFlags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP | (IsExtended(vk) ? KEYEVENTF_EXTENDEDKEY : 0);
        INPUT[] oneUp = new INPUT[] { up };
        SendInput(1, oneUp, Marshal.SizeOf(typeof(INPUT)));
    }

    /// 组合键：按 vks 顺序按下，再倒序抬起（例如 Alt+Space = [0x12, 0x20]）。
    public static void Hotkey(ushort[] vks) {
        if (vks == null) return;
        foreach (ushort vk in vks) {
            INPUT down = new INPUT();
            down.type = INPUT_KEYBOARD;
            down.u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
            down.u.ki.dwFlags = KEYEVENTF_SCANCODE | (IsExtended(vk) ? KEYEVENTF_EXTENDEDKEY : 0);
            INPUT[] one = new INPUT[] { down };
            SendInput(1, one, Marshal.SizeOf(typeof(INPUT)));
            System.Threading.Thread.Sleep(30);
        }
        for (int i = vks.Length - 1; i >= 0; i--) {
            ushort vk = vks[i];
            INPUT up = new INPUT();
            up.type = INPUT_KEYBOARD;
            up.u.ki.wScan = (ushort)MapVirtualKey(vk, 0);
            up.u.ki.dwFlags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP | (IsExtended(vk) ? KEYEVENTF_EXTENDEDKEY : 0);
            INPUT[] oneUp = new INPUT[] { up };
            SendInput(1, oneUp, Marshal.SizeOf(typeof(INPUT)));
            System.Threading.Thread.Sleep(30);
        }
    }

    static bool IsExtended(ushort vk) {
        switch (vk) {
            case 0x21: case 0x22: case 0x23: case 0x24: case 0x25: case 0x26: case 0x27: case 0x28:
            case 0x2D: case 0x2E: case 0x5B: case 0x5C: case 0x5D: case 0x6F: case 0x90:
                return true;
            default:
                return false;
        }
    }

    /// 截窗口区域（物理像素），返回 "宽x高"。
    public static string Capture(IntPtr h, string path) {
        if (h == IntPtr.Zero) return "no window";
        RECT r = Rect(h);
        int w = r.R - r.L, hh = r.B - r.T;
        if (w <= 0 || hh <= 0) return "bad rect";
        using (Bitmap bmp = new Bitmap(w, hh)) {
            using (Graphics g = Graphics.FromImage(bmp)) {
                g.CopyFromScreen(r.L, r.T, 0, 0, new Size(w, hh));
            }
            bmp.Save(path, ImageFormat.Png);
        }
        return w + "x" + hh;
    }
}
"@
    Add-Type -TypeDefinition $neoWinCode -ReferencedAssemblies System.Drawing
}
[NeoWin]::InitDpi()

$script:NeoExe = "E:\NeoSonia\target\debug\neosonia.exe"
$script:NeoHome = "E:\NeoSonia\sandbox"
$script:NeoLog = Join-Path $script:NeoHome "logs\neosonia.log"

# ============================================================
# 窗口 / 坐标
# ============================================================

function Get-NeoWindow {
    <# 返回窗口信息；窗口不在时返回 $null。 #>
    $h = [NeoWin]::Find()
    if ($h -eq [IntPtr]::Zero) {
        # 窗口刚创建/重建时可能瞬间找不到，重试一次
        Start-Sleep -Milliseconds 150
        $h = [NeoWin]::Find()
    }
    if ($h -eq [IntPtr]::Zero) { return $null }
    $r = [NeoWin]::Rect($h)
    $dpi = [NeoWin]::Dpi($h)
    $scale = $dpi / 96.0
    [pscustomobject]@{
        Hwnd          = $h
        Left          = $r.L
        Top           = $r.T
        Right         = $r.R
        Bottom        = $r.B
        Width         = $r.R - $r.L
        Height        = $r.B - $r.T
        Dpi           = $dpi
        Scale         = $scale
        LogicalWidth  = [math]::Round(($r.R - $r.L) / $scale, 1)
        LogicalHeight = [math]::Round(($r.B - $r.T) / $scale, 1)
    }
}

function Invoke-NeoFocus {
    <# 强制把 app 窗口拉到前台。 #>
    $h = [NeoWin]::Find()
    if ($h -eq [IntPtr]::Zero) { return $false }
    $ok = [NeoWin]::Focus($h)
    Start-Sleep -Milliseconds 400
    return $ok
}

function Invoke-NeoClick {
    <# 左键点击：参数是相对窗口左上角的“物理像素”。 #>
    param([Parameter(Mandatory = $true)][int]$hwndRelX, [Parameter(Mandatory = $true)][int]$hwndRelY)
    $win = Get-NeoWindow
    if (-not $win) { throw "NeoSonia 窗口不存在，无法点击" }
    $x = $win.Left + $hwndRelX
    $y = $win.Top + $hwndRelY
    [NeoWin]::Click($x, $y)
    Start-Sleep -Milliseconds 250
    return ("click phys rel(" + $hwndRelX + "," + $hwndRelY + ") -> screen(" + $x + "," + $y + ")")
}

function Invoke-NeoClickLogical {
    <# 左键点击：参数是“逻辑像素”（相对窗口左上角，96 DPI 基准），内部按 DPI 缩放。 #>
    param([Parameter(Mandatory = $true)][double]$LogicalX, [Parameter(Mandatory = $true)][double]$LogicalY)
    $win = Get-NeoWindow
    if (-not $win) { throw "NeoSonia 窗口不存在，无法点击" }
    $x = [int][math]::Round($LogicalX * $win.Scale)
    $y = [int][math]::Round($LogicalY * $win.Scale)
    [NeoWin]::Click($win.Left + $x, $win.Top + $y)
    Start-Sleep -Milliseconds 250
    return ("click logical(" + $LogicalX + "," + $LogicalY + ") -> phys rel(" + $x + "," + $y + ")")
}

function Invoke-NeoWheel {
    <#
      在窗口中部滚动：正数向下、负数向上。
      说明：fenestra 的滚动容器要“鼠标真的 hover 过”才认滚轮事件，所以要先把光标移进窗口
      并发一次 MOVE 事件（见 C# NeoWin.Hover），否则会出现“滚了但页面没动”。
      PageDown/PageUp 键盘滚动备选见 Send-NeoKeyScroll。
    #>
    param([Parameter(Mandatory = $true)][int]$Notches)
    $win = Get-NeoWindow
    if (-not $win) { throw "NeoSonia 窗口不存在，无法滚动" }
    # 滚轮事件发给光标所在窗口，先把光标 hover 到内容区中部
    $cx = $win.Left + [int]($win.Width / 2)
    $cy = $win.Top + [int]($win.Height / 2)
    [NeoWin]::Hover($cx, $cy)
    # 大跨度滚动分块发（每块 <=6 格），避免单次 wheel delta 过大被系统/框架丢弃
    $remaining = [math]::Abs($Notches)
    $sign = 1
    if ($Notches -lt 0) { $sign = -1 }
    while ($remaining -gt 0) {
        $chunk = [math]::Min(6, $remaining)
        [NeoWin]::Wheel($sign * $chunk)
        $remaining -= $chunk
        if ($remaining -gt 0) { Start-Sleep -Milliseconds 70 }
    }
    Start-Sleep -Milliseconds 400
}

function Send-NeoKeyScroll {
    <#
      键盘滚动备选：PageDown / PageUp / Home / End（部分滚动容器要先把焦点给内容区，
      必要时先 Invoke-NeoClick 点一下内容区空白处）。
    #>
    param([Parameter(Mandatory = $true)][ValidateSet("PageDown", "PageUp", "Home", "End")][string]$Key)
    $vk = 0
    switch ($Key) {
        "PageDown" { $vk = 0x22 }
        "PageUp"   { $vk = 0x21 }
        "Home"     { $vk = 0x24 }
        "End"      { $vk = 0x23 }
    }
    $win = Get-NeoWindow
    if (-not $win) { throw "NeoSonia 窗口不存在，无法发送按键" }
    [NeoWin]::Hover($win.Left + [int]($win.Width / 2), $win.Top + [int]($win.Height / 2))
    [NeoWin]::Key([uint16]$vk)
    Start-Sleep -Milliseconds 400
}

function Save-NeoScreenshot {
    <# 截 app 窗口区域到 PNG，返回路径。父目录会自动创建。 #>
    param([Parameter(Mandatory = $true)][string]$Path)
    $win = Get-NeoWindow
    if (-not $win) { throw "NeoSonia 窗口不存在，无法截图" }
    $dir = Split-Path -Parent $Path
    if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
    $size = [NeoWin]::Capture($win.Hwnd, $Path)
    if (-not (Test-Path $Path)) { throw ("截图失败：" + $size) }
    return [pscustomobject]@{ Path = $Path; Size = $size }
}

# ============================================================
# 输入模拟
# ============================================================

function Send-NeoKeys {
    <# 用 SendInput(KEYEVENTF_UNICODE) 打一串文本（中文也可以）。 #>
    param([Parameter(Mandatory = $true)][string]$Text)
    [NeoWin]::TypeText($Text)
    Start-Sleep -Milliseconds 120
    return ("typed " + $Text.Length + " chars")
}

function Send-NeoHotkey {
    <#
      发送组合键，例如 "Alt+Space"、"ctrl+alt+a"。
      注意：无法触发 NeoSonia 自己的全局快捷键（键盘钩子忽略注入事件），见文件头说明。
    #>
    param([Parameter(Mandatory = $true)][string]$Spec)
    $parts = $Spec -split "\+"
    $vks = @()
    foreach ($raw in $parts) {
        $name = $raw.Trim().ToUpperInvariant()
        switch ($name) {
            "CTRL"      { $vks += 0x11 }
            "CONTROL"   { $vks += 0x11 }
            "ALT"       { $vks += 0x12 }
            "SHIFT"     { $vks += 0x10 }
            "WIN"       { $vks += 0x5B }
            "SPACE"     { $vks += 0x20 }
            "TAB"       { $vks += 0x09 }
            "ENTER"     { $vks += 0x0D }
            "ESC"       { $vks += 0x1B }
            "ESCAPE"    { $vks += 0x1B }
            "LEFT"      { $vks += 0x25 }
            "UP"        { $vks += 0x26 }
            "RIGHT"     { $vks += 0x27 }
            "DOWN"      { $vks += 0x28 }
            "HOME"      { $vks += 0x24 }
            "END"       { $vks += 0x23 }
            "PAGEUP"    { $vks += 0x21 }
            "PAGEDOWN"  { $vks += 0x22 }
            "DELETE"    { $vks += 0x2E }
            "BACKSPACE" { $vks += 0x08 }
            default {
                if ($name.Length -eq 1) {
                    $code = [int][char]$name
                    if ($code -ge 0x41 -and $code -le 0x5A) { $vks += $code }
                    elseif ($code -ge 0x30 -and $code -le 0x39) { $vks += $code }
                    else { throw ("不认识的按键：" + $name) }
                } else {
                    throw ("不认识的按键：" + $name)
                }
            }
        }
    }
    [NeoWin]::Hotkey([uint16[]]$vks)
    Start-Sleep -Milliseconds 200
    return ("hotkey sent: " + $Spec + "（注意：不会触发 NeoSonia 自己的钩子热键）")
}

function Send-NeoKey {
    <# 发送单个虚拟键（如 Enter=0x0D）。 #>
    param([Parameter(Mandatory = $true)][int]$VirtualKey)
    [NeoWin]::Key([uint16]$VirtualKey)
    Start-Sleep -Milliseconds 120
}

# ============================================================
# 日志 / 等待
# ============================================================

function Test-NeoHotkeyHookInstalled {
    <#
      通过日志判断全局快捷键是否已安装：
      - “全局快捷键已启用（键盘钩子）” = 真钩子（最理想）；
      - “全局快捷键已注册（退回模式）” = global-hotkey 退回实现。
      返回 $null 表示日志里没有相关记录。
    #>
    param([string]$LogPath = $script:NeoLog)
    if (-not (Test-Path $LogPath)) { return $null }
    $hook = Select-String -Path $LogPath -Pattern "全局快捷键已启用（键盘钩子）" -Quiet -ErrorAction SilentlyContinue
    if ($hook) { return "键盘钩子" }
    $register = Select-String -Path $LogPath -Pattern "全局快捷键已注册" -Quiet -ErrorAction SilentlyContinue
    if ($register) { return "RegisterHotKey（新实现）" }
    $fallback = Select-String -Path $LogPath -Pattern "全局快捷键已注册（退回模式）" -Quiet -ErrorAction SilentlyContinue
    if ($fallback) { return "退回模式（global-hotkey）" }
    return $null
}

function Get-NeoLogLineCount {
    <# 当前日志行数（用于“只看本次操作之后新增的日志”）。 #>
    param([string]$LogPath = $script:NeoLog)
    if (-not (Test-Path $LogPath)) { return 0 }
    return @(Get-Content $LogPath -ErrorAction SilentlyContinue).Count
}

function Wait-NeoLog {
    <# 等日志里出现某条匹配（从调用时刻起），返回匹配行；超时返回 $null。 #>
    param(
        [Parameter(Mandatory = $true)][string]$Pattern,
        [string]$LogPath = $script:NeoLog,
        [int]$TimeoutSec = 20
    )
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $LogPath) {
            $hit = Select-String -Path $LogPath -Pattern $Pattern -ErrorAction SilentlyContinue | Select-Object -Last 1
            if ($hit) { return $hit.Line }
        }
        Start-Sleep -Milliseconds 200
    }
    return $null
}

function Wait-NeoWindow {
    <# 等 app 窗口出现。 #>
    param([int]$TimeoutSec = 40)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ([NeoWin]::Find() -ne [IntPtr]::Zero) {
            Start-Sleep -Milliseconds 600
            return $true
        }
        Start-Sleep -Milliseconds 200
    }
    return $false
}

function Wait-NeoDock {
    <# 等日志出现 dock attempt（首帧 + 自动停靠完成）。 #>
    param([string]$LogPath = $script:NeoLog, [int]$TimeoutSec = 40)
    return [bool](Wait-NeoLog -Pattern "dock attempt" -LogPath $LogPath -TimeoutSec $TimeoutSec)
}

function Wait-NeoPage {
    <#
      等日志出现“page switch page=<Page>”，用于确认导航点击生效。
      -SinceLine：只检查这一行号之后新增的日志（默认 -1 = 从当前日志末尾开始，
      避免把点击之前的旧 page switch 误当成这次点击的结果）。
    #>
    param(
        [Parameter(Mandatory = $true)][string]$Page,
        [string]$LogPath = $script:NeoLog,
        [int]$TimeoutSec = 10,
        [int]$SinceLine = -1
    )
    $pattern = "page switch page=" + [regex]::Escape($Page) + "\b"
    if ($SinceLine -lt 0) { $SinceLine = Get-NeoLogLineCount -LogPath $LogPath }
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $LogPath) {
            $all = @(Get-Content $LogPath -ErrorAction SilentlyContinue)
            if ($all.Count -gt $SinceLine) {
                $newLines = $all[$SinceLine..($all.Count - 1)]
                $hit = $newLines | Select-String -Pattern $pattern -Quiet -ErrorAction SilentlyContinue
                if ($hit) { return $true }
            }
        }
        Start-Sleep -Milliseconds 100
    }
    return $false
}

function Get-NeoLogLast {
    <# 取日志最后 N 行（默认 40）。 #>
    param([string]$LogPath = $script:NeoLog, [int]$Lines = 40)
    if (-not (Test-Path $LogPath)) { return @() }
    return @(Get-Content $LogPath -Tail $Lines -ErrorAction SilentlyContinue)
}

# ============================================================
# app 生命周期
# ============================================================

function Start-NeoApp {
    <#
      启动 app 并等到 dock 完成，返回 app 句柄对象：
        Process / Home / Log / StartupMs / DockLine / Window
      参数：
        -Home     app 的 --home（默认 E:\NeoSonia\sandbox）
        -Profile  是否设置 NEOSONIA_PROFILE=1（perf 脚本要真实帧率日志）
        -WaitDock 是否等待 dock（默认是；e2e 需要）
      会先清掉同名进程和旧日志，保证冷启动可重复。
    #>
    param(
        [string]$HomeDir = $script:NeoHome,
        [switch]$Profile,
        [switch]$KeepLog,
        [int]$DockTimeoutSec = 60
    )
    $exe = $script:NeoExe
    if (-not (Test-Path $exe)) { throw ("找不到 app 可执行文件：" + $exe) }
    # 清掉旧实例并确认真的退出了（并行/多实例环境下必须确认，否则下一个实例会和旧窗口抢 FindWindow）
    Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
    for ($i = 0; $i -lt 24; $i++) {
        Start-Sleep -Milliseconds 150
        if (-not (Get-Process neosonia -ErrorAction SilentlyContinue)) { break }
        Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
    }
    Start-Sleep -Milliseconds 300
    $logPath = Join-Path $HomeDir "logs\neosonia.log"
    if (-not $KeepLog) { Remove-Item $logPath -ErrorAction SilentlyContinue }
    Remove-Item Env:NEOSONIA_PAGE -ErrorAction SilentlyContinue
    if ($Profile) { $env:NEOSONIA_PROFILE = "1" } else { Remove-Item Env:NEOSONIA_PROFILE -ErrorAction SilentlyContinue }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $proc = Start-Process -FilePath $exe -ArgumentList @("--home", $HomeDir) -PassThru
    $dockLine = Wait-NeoLog -Pattern "dock attempt" -LogPath $logPath -TimeoutSec $DockTimeoutSec
    $sw.Stop()
    $win = Get-NeoWindow
    return [pscustomobject]@{
        Process   = $proc
        Home      = $HomeDir
        Log       = $logPath
        StartupMs = [int]$sw.ElapsedMilliseconds
        DockLine  = $dockLine
        Window    = $win
        Profile   = [bool]$Profile
    }
}

function Get-NeoChildProcessIds {
    <# 递归收集某个进程的子进程 id（默认 2 层，够覆盖 app -> node MCP server）。 #>
    param([int]$ParentId, [int]$Depth = 2)
    $ids = @()
    $frontier = @($ParentId)
    for ($level = 0; $level -lt $Depth; $level++) {
        $next = @()
        foreach ($parent in $frontier) {
            $kids = @(Get-CimInstance Win32_Process -Filter ("ParentProcessId=" + $parent) -ErrorAction SilentlyContinue)
            foreach ($kid in $kids) {
                $ids += [int]$kid.ProcessId
                $next += [int]$kid.ProcessId
            }
        }
        $frontier = $next
        if ($frontier.Count -eq 0) { break }
    }
    return $ids
}

function Stop-NeoApp {
    <#
      结束 app：先递归记下 app 的子进程（含 MCP 的 node 子进程），杀掉 app 后再清掉这些孤儿，
      保证脚本幂等、不留后台进程。
      注意：只清“本 app 的子进程”，不会误杀别的 NeoSonia/node 实例（并行测试时很重要）。
    #>
    param($App)
    $pidToKill = 0
    $children = @()
    if ($App -and $App.Process) {
        $pidToKill = [int]$App.Process.Id
        $children = @(Get-NeoChildProcessIds -ParentId $pidToKill)
    }
    if ($pidToKill -eq 0) {
        $running = Get-Process neosonia -ErrorAction SilentlyContinue
        if ($running) {
            $pidToKill = [int]$running[0].Id
            $children = @(Get-NeoChildProcessIds -ParentId $pidToKill)
        }
    }
    if ($pidToKill -eq 0) { return }
    $proc = Get-Process -Id $pidToKill -ErrorAction SilentlyContinue
    if ($proc -and -not $proc.HasExited) { $proc.Kill() }
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 100
        $still = Get-Process -Id $pidToKill -ErrorAction SilentlyContinue
        if (-not $still) { break }
    }
    foreach ($childPid in $children) {
        $child = Get-Process -Id $childPid -ErrorAction SilentlyContinue
        if ($child -and -not $child.HasExited) {
            try { $child.Kill() } catch { }
        }
    }
    Start-Sleep -Milliseconds 300
}
