$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class P5 {
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
  public static int X(){ IntPtr h=Find(); if(h==IntPtr.Zero) return -99999; RECT r; GetWindowRect(h,out r); return r.L; }
  public static string Click(int rx, int ry){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    SetForegroundWindow(h); System.Threading.Thread.Sleep(100);
    SetCursorPos(r.L+rx, r.T+ry); System.Threading.Thread.Sleep(60);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(25);
    mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(25);
    mouse_event(0x0004,0,0,0,IntPtr.Zero);
    return "ok";
  }
  public static void Down(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  public static void Up(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; a[0].u.ki.dwFlags=2; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int cb);
  public static void AltSpace(){ Down(0xA4); System.Threading.Thread.Sleep(35); Down(0x20); System.Threading.Thread.Sleep(35); Up(0x20); System.Threading.Thread.Sleep(35); Up(0xA4); }
}
"@ -ReferencedAssemblies System.Drawing

$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$env:NEOSONIA_FRAME_LOG = "1"
$env:NEOSONIA_PROFILE = "1"

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
Remove-Item "E:\NeoSonia\logs\perf-frame2.err" -ErrorAction SilentlyContinue

$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru -RedirectStandardError "E:\NeoSonia\logs\perf-frame2.err"
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 2000

# ---------- 1) 切页延迟 + 每页 paint ----------
$nav = @(@(19,"Chat"), @(62,"Mail"), @(106,"Memory"), @(157,"Persona"), @(201,"Settings"), @(19,"Chat"))
foreach ($n in $nav) {
    [void][P5]::Click([int]($n[0] * 2.5), [int](68 * 2.5))
    Start-Sleep -Milliseconds 800
}
[void][P5]::Click([int](288 * 2.5), [int](24 * 2.5)); Start-Sleep -Milliseconds 800

# ---------- 2) 显隐延迟 ----------
$hide = @(); $show = @()
for ($k = 0; $k -lt 3; $k++) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew(); [P5]::AltSpace()
    while ($sw.ElapsedMilliseconds -lt 3000) { if ([P5]::X() -lt -5000) { break }; Start-Sleep -Milliseconds 4 }
    $hide += $sw.ElapsedMilliseconds
    Start-Sleep -Milliseconds 200
    $sw2 = [System.Diagnostics.Stopwatch]::StartNew(); [P5]::AltSpace()
    while ($sw2.ElapsedMilliseconds -lt 3000) { if ([P5]::X() -gt 0) { break }; Start-Sleep -Milliseconds 4 }
    $show += $sw2.ElapsedMilliseconds
    Start-Sleep -Milliseconds 250
}
"hide ms: $($hide -join ', ')"
"show ms: $($show -join ', ')"

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800

# ---------- 3) 解析帧日志（按时间分组） ----------
$frames = @()
foreach ($line in Get-Content "E:\NeoSonia\logs\perf-frame2.err" -ErrorAction SilentlyContinue) {
    $m = [regex]::Match($line, '\[frame t=(\d+)\] total=([\d\.]+)ms view=([\d\.]+)ms layout=([\d\.]+)ms paint=([\d\.]+)ms present=([\d\.]+)ms')
    if ($m.Success) {
        $frames += [pscustomobject]@{ t=[long]$m.Groups[1].Value; total=[double]$m.Groups[2].Value; view=[double]$m.Groups[3].Value; layout=[double]$m.Groups[4].Value; paint=[double]$m.Groups[5].Value; present=[double]$m.Groups[6].Value }
    }
}
function Stat($name, $vals) {
    if (-not $vals -or $vals.Count -eq 0) { return "${name}: (no data)" }
    $s = @($vals | Sort-Object)
    $p50 = $s[[int]($s.Count * 0.5)]
    $p95 = $s[[Math]::Min($s.Count - 1, [int]($s.Count * 0.95))]
    return ("{0}: n={1} avg={2:N1} p50={3:N1} p95={4:N1} max={5:N1}" -f $name, $s.Count, (($s | Measure-Object -Average).Average), $p50, $p95, $s[-1])
}
"frames=$($frames.Count)"
Stat "total  " ($frames | ForEach-Object { $_.total })
Stat "view   " ($frames | ForEach-Object { $_.view })
Stat "layout " ($frames | ForEach-Object { $_.layout })
Stat "paint  " ($frames | ForEach-Object { $_.paint })
Stat "present" ($frames | ForEach-Object { $_.present })
Write-Host "=== page switch ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "page switch" | ForEach-Object { $_.Line } | Select-Object -Last 10
Remove-Item Env:NEOSONIA_FRAME_LOG -ErrorAction SilentlyContinue
Remove-Item Env:NEOSONIA_PROFILE -ErrorAction SilentlyContinue
