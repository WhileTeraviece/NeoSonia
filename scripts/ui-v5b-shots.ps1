$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public class V6 {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x,int y,int cx,int cy,uint f);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  static IntPtr Find(){ SetProcessDPIAware(); return FindWindowW(null,"NeoSonia"); }
  public static bool Vis(){ IntPtr h=Find(); return h!=IntPtr.Zero && IsWindowVisible(h); }
  public static string Shot(string path){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    SetWindowPos(h,new IntPtr(-1),0,0,0,0,0x0001|0x0002); System.Threading.Thread.Sleep(300);
    RECT r; GetWindowRect(h,out r);
    using(Bitmap bmp=new Bitmap(r.R-r.L,r.B-r.T)){ using(Graphics g=Graphics.FromImage(bmp)){ g.CopyFromScreen(r.L,r.T,0,0,new Size(r.R-r.L,r.B-r.T)); } bmp.Save(path,System.Drawing.Imaging.ImageFormat.Png);} return "saved";
  }
  public static string Click(int rx, int ry){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    SetForegroundWindow(h); System.Threading.Thread.Sleep(150);
    SetCursorPos(r.L+rx, r.T+ry); System.Threading.Thread.Sleep(90);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(30);
    mouse_event(0x0004,0,0,0,IntPtr.Zero);
    return "ok";
  }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public InputUnion u; }
  [StructLayout(LayoutKind.Explicit)] public struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public HARDWAREINPUT hi; }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct HARDWAREINPUT { public uint uMsg; public ushort wParamL; public ushort wParamH; }
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int cb);
  static void Down(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  static void Up(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; a[0].u.ki.dwFlags=2; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  public static void AltSpace(){ Down(0xA4); System.Threading.Thread.Sleep(35); Down(0x20); System.Threading.Thread.Sleep(35); Up(0x20); System.Threading.Thread.Sleep(35); Up(0xA4); }
  public static string Wheel(int notches){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    SetForegroundWindow(h); System.Threading.Thread.Sleep(120);
    SetCursorPos(r.L+(r.R-r.L)/2, r.T+(r.B-r.T)/2); System.Threading.Thread.Sleep(100);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(60);
    for(int i=0;i<Math.Abs(notches);i++){ mouse_event(0x0800,0,0,notches>0?120:-120,IntPtr.Zero); System.Threading.Thread.Sleep(50); }
    return "ok";
  }
}
"@ -ReferencedAssemblies System.Drawing

$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$out = "E:\NeoSonia\logs\ui\v5"
Get-ChildItem "E:\NeoSonia\sandbox\personas" -ErrorAction SilentlyContinue | Select-Object Name | Out-Null

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 1800

# ---- 显隐延迟（用 IsWindowVisible 判断）----
$hide = @(); $show = @()
for ($k = 0; $k -lt 3; $k++) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    [V6]::AltSpace()
    while ($sw.ElapsedMilliseconds -lt 2500) { if (-not [V6]::Vis()) { break }; Start-Sleep -Milliseconds 4 }
    $hide += $sw.ElapsedMilliseconds
    Start-Sleep -Milliseconds 150
    $sw2 = [System.Diagnostics.Stopwatch]::StartNew()
    [V6]::AltSpace()
    while ($sw2.ElapsedMilliseconds -lt 2500) { if ([V6]::Vis()) { break }; Start-Sleep -Milliseconds 4 }
    $show += $sw2.ElapsedMilliseconds
    Start-Sleep -Milliseconds 250
}
"hide ms: $($hide -join ', ')"
"show ms: $($show -join ', ')"
# ---- 设置页：上下文压缩/联网搜索 ----
[void][V6]::Click([int](201*2.5), [int](68*2.5)); Start-Sleep -Milliseconds 1200
[void][V6]::Wheel(-9); Start-Sleep -Milliseconds 700
"07 settings compression/search: $([V6]::Shot("$out\07-settings-compress.png"))"
[void][V6]::Wheel(-8); Start-Sleep -Milliseconds 700
"08 settings search/mail: $([V6]::Shot("$out\08-settings-search-mail.png"))"

# ---- 人设：新建人设 ----
[void][V6]::Click([int](157*2.5), [int](68*2.5)); Start-Sleep -Milliseconds 1200
[void][V6]::Click(590, 350); Start-Sleep -Milliseconds 1000
"09 persona editor: $([V6]::Shot("$out\09-persona-editor.png"))"

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "=== page switch ==="; Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "page switch" | ForEach-Object { $_.Line } | Select-Object -Last 4
Write-Host "=== max_tokens in settings.toml ==="; Select-String -Path "E:\NeoSonia\sandbox\config\settings.toml" -Pattern "max_tokens|context_compress|search_provider" | ForEach-Object { $_.Line.Trim() }
