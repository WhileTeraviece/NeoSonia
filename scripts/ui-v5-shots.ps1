$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public class V5 {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr a, int x,int y,int cx,int cy,uint f);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  static IntPtr Find(){ SetProcessDPIAware(); return FindWindowW(null,"NeoSonia"); }
  public static string Shot(string path){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    SetWindowPos(h,new IntPtr(-1),0,0,0,0,0x0001|0x0002); System.Threading.Thread.Sleep(280);
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
    mouse_event(0x0004,0,0,0,IntPtr.Zero);
    return "ok";
  }
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
New-Item -ItemType Directory -Force $out | Out-Null

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 2000
"app pid=$($p.Id)"

"01 chat/logo: $([V5]::Shot("$out\01-chat-logo.png"))"
[void][V5]::Click([int](201*2.5), [int](68*2.5))
Start-Sleep -Milliseconds 60
"02 loading: $([V5]::Shot("$out\02-page-loading.png"))"
Start-Sleep -Milliseconds 1500
"03 settings top: $([V5]::Shot("$out\03-settings-top.png"))"
[void][V5]::Wheel(-4)
Start-Sleep -Milliseconds 600
"04 settings token: $([V5]::Shot("$out\04-settings-token.png"))"
[void][V5]::Click([int](157*2.5), [int](68*2.5))
Start-Sleep -Milliseconds 1500
"05 persona list: $([V5]::Shot("$out\05-persona-list.png"))"
[void][V5]::Click([int](388*2.5), [int](104*2.5))
Start-Sleep -Milliseconds 900
"06 persona editor: $([V5]::Shot("$out\06-persona-editor.png"))"

Write-Host "=== page switch ms ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "page switch" | ForEach-Object { $_.Line } | Select-Object -Last 8
Write-Host "=== warmup ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "字形预热" | ForEach-Object { $_.Line }
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
