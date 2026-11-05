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
  public static string Shot(string path){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    SetWindowPos(h,new IntPtr(-1),0,0,0,0,0x0001|0x0002); System.Threading.Thread.Sleep(300);
    RECT r; GetWindowRect(h,out r);
    using(Bitmap bmp=new Bitmap(r.R-r.L,r.B-r.T)){ using(Graphics g=Graphics.FromImage(bmp)){ g.CopyFromScreen(r.L,r.T,0,0,new Size(r.R-r.L,r.B-r.T)); } bmp.Save(path,System.Drawing.Imaging.ImageFormat.Png);} return "saved";
  }
  public static string Click(int rx, int ry){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    SetForegroundWindow(h); System.Threading.Thread.Sleep(120);
    SetCursorPos(r.L+rx, r.T+ry); System.Threading.Thread.Sleep(80);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(25);
    mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(25);
    mouse_event(0x0004,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(250);
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
  public static void CtrlAltA(){ Down(0xA2); Down(0xA4); System.Threading.Thread.Sleep(40); Down(0x41); System.Threading.Thread.Sleep(40); Up(0x41); System.Threading.Thread.Sleep(40); Up(0xA4); Up(0xA2); }
  public static string Vis(){ IntPtr h=Find(); if(h==IntPtr.Zero) return "none"; return IsWindowVisible(h)?"visible":"hidden"; }
}
"@ -ReferencedAssemblies System.Drawing

$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$out = "E:\NeoSonia\logs\ui\v6"
New-Item -ItemType Directory -Force $out | Out-Null

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
Remove-Item "E:\NeoSonia\sandbox\data\screenshots\shot-*.png" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 2500

# 1) LAN 服务自检（用本机 LAN IP，不用 127.0.0.1）
$lan = (Get-NetIPAddress -AddressFamily IPv4 | Where-Object { $_.IPAddress -like "192.168.*" } | Select-Object -First 1).IPAddress
"lan=$lan"
try { $h = Invoke-WebRequest -Uri "http://${lan}:8787/healthz" -UseBasicParsing -TimeoutSec 5; "healthz=$($h.StatusCode) body=$($h.Content.Substring(0,[Math]::Min(160,$h.Content.Length)))" } catch { "healthz FAIL: $($_.Exception.Message)" }
Write-Host "=== app log web ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "LAN web server|防火墙|远程访问" | ForEach-Object { $_.Line }

# 2) 多行输入框 + 顶栏截图
"chat: $([V6]::Shot("$out\01-composer.png"))"

# 3) 截图热键：先截图再开窗口（先把窗口隐藏，然后按 Ctrl+Alt+A，截图完成应自动打开窗口并显示附件）
[void][V6]::Click(-9999, -9999)
# 用托盘/热键隐藏：Alt+Space 由 hotkey 处理（这里简化：直接调用 窗口隐藏 按钮 x=401,y=24 物理）
[void][V6]::Click([int](401*2.5), [int](24*2.5))
Start-Sleep -Milliseconds 500
"after hide: $([V6]::Vis())"
[V6]::CtrlAltA()
Start-Sleep -Seconds 4
"after screenshot: vis=$([V6]::Vis()) shots=$((Get-ChildItem "E:\NeoSonia\sandbox\data\screenshots\shot-*.png" -ErrorAction SilentlyContinue).Count)"
"shot attached: $([V6]::Shot("$out\02-shot-order.png"))"

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Write-Host "=== page switch / hotkey ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "hotkey dispatch|page switch" | ForEach-Object { $_.Line } | Select-Object -Last 6
