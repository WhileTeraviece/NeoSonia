$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class PERF {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public InputUnion u; }
  [StructLayout(LayoutKind.Explicit)] public struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public HARDWAREINPUT hi; }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct HARDWAREINPUT { public uint uMsg; public ushort wParamL; public ushort wParamH; }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  static IntPtr Find(){ SetProcessDPIAware(); return FindWindowW(null,"NeoSonia"); }
  public static string Rect(){ IntPtr h=Find(); if(h==IntPtr.Zero) return "none"; RECT r; GetWindowRect(h,out r); return r.L+"|"+r.T+"|"+(r.R-r.L)+"|"+(r.B-r.T); }
  public static string Click(int rx, int ry){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    SetForegroundWindow(h); System.Threading.Thread.Sleep(120);
    SetCursorPos(r.L+rx, r.T+ry); System.Threading.Thread.Sleep(80);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x0004,0,0,0,IntPtr.Zero);
    return "ok";
  }
  public static void Down(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  public static void Up(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; a[0].u.ki.dwFlags=2; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int cb);
  public static void AltSpace(){ Down(0xA4); System.Threading.Thread.Sleep(40); Down(0x20); System.Threading.Thread.Sleep(40); Up(0x20); System.Threading.Thread.Sleep(40); Up(0xA4); }
}
"@ -ReferencedAssemblies System.Drawing

$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$env:NEOSONIA_FRAME_LOG = "1"
$env:NEOSONIA_PROFILE = "1"

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
Remove-Item "E:\NeoSonia\logs\perf-frame.err" -ErrorAction SilentlyContinue

$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru -RedirectStandardError "E:\NeoSonia\logs\perf-frame.err"
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 1500
"app started pid=$($p.Id) rect=$([PERF]::Rect())"

# ---- 页面切换：点 5 个导航（逻辑坐标 x*2.5, y=68*2.5） ----
$nav = @(@(19,"Chat"), @(62,"Mail"), @(106,"Memory"), @(157,"Persona"), @(201,"Settings"))
foreach ($n in $nav) {
    [void][PERF]::Click([int]($n[0] * 2.5), [int](68 * 2.5))
    Start-Sleep -Milliseconds 500
}
# 点回对话 + 设备按钮
[void][PERF]::Click([int](19 * 2.5), [int](68 * 2.5)); Start-Sleep -Milliseconds 500
[void][PERF]::Click([int](288 * 2.5), [int](24 * 2.5)); Start-Sleep -Milliseconds 800
[void][PERF]::Click([int](19 * 2.5), [int](68 * 2.5)); Start-Sleep -Milliseconds 500

# ---- 显示/隐藏延迟 ----
$lat = @()
for ($k = 0; $k -lt 3; $k++) {
    $before = [PERF]::Rect()
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    [PERF]::AltSpace()
    $hidden = $false
    while ($sw.ElapsedMilliseconds -lt 2000) {
        $r = [PERF]::Rect()
        $x = [int](($r -split '\|')[0])
        if ($r -ne $before -and $x -lt -5000) { $hidden = $true; break }
        Start-Sleep -Milliseconds 5
    }
    $sw.Stop()
    $hideMs = $sw.ElapsedMilliseconds
    Start-Sleep -Milliseconds 250
    $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
    [PERF]::AltSpace()
    $shown = $false
    while ($sw2.ElapsedMilliseconds -lt 2000) {
        $r = [PERF]::Rect()
        $x = [int](($r -split '\|')[0])
        if ($r -ne $before -and $x -gt 0) { $shown = $true; break }
        Start-Sleep -Milliseconds 5
    }
    $sw2.Stop()
    $lat += "hide=$hideMs ms show=$($sw2.ElapsedMilliseconds) ms"
    Start-Sleep -Milliseconds 300
}
"toggle latency: " + ($lat -join " | ")

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

# ---- 解析 frame 日志 ----
$frames = @()
foreach ($line in Get-Content "E:\NeoSonia\logs\perf-frame.err" -ErrorAction SilentlyContinue) {
    $m = [regex]::Match($line, '[frame] total=([\d\.]+)ms view=([\d\.]+)ms layout=([\d\.]+)ms paint=([\d\.]+)ms present=([\d\.]+)ms')
    if ($m.Success) {
        $frames += [pscustomobject]@{ total = [double]$m.Groups[1].Value; view = [double]$m.Groups[2].Value; layout = [double]$m.Groups[3].Value; paint = [double]$m.Groups[4].Value; present = [double]$m.Groups[5].Value }
    }
}
function Stat($name, $vals) {
    if ($vals.Count -eq 0) { return "$name`: (no data)" }
    $s = $vals | Sort-Object
    $p50 = $s[[int]($s.Count * 0.5)]
    $p95 = $s[[Math]::Min($s.Count - 1, [int]($s.Count * 0.95))]
    "{0}: n={1} avg={2:N1} p50={3:N1} p95={4:N1} max={5:N1}" -f $name, $s.Count, (($s | Measure-Object -Average).Average), $p50, $p95, ($s[-1])
}
"frames=$($frames.Count)"
Stat "frame total" ($frames | ForEach-Object { $_.total })
Stat "  view    " ($frames | ForEach-Object { $_.view })
Stat "  layout  " ($frames | ForEach-Object { $_.layout })
Stat "  paint   " ($frames | ForEach-Object { $_.paint })
Stat "  present " ($frames | ForEach-Object { $_.present })
Write-Host "=== page switch 日志 ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "page switch" | ForEach-Object { $_.Line } | Select-Object -Last 14
Remove-Item Env:NEOSONIA_FRAME_LOG -ErrorAction SilentlyContinue
Remove-Item Env:NEOSONIA_PROFILE -ErrorAction SilentlyContinue
