$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Drawing;
using System.Runtime.InteropServices;
public class SHS {
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
    SetWindowPos(h,new IntPtr(-1),0,0,0,0,0x0001|0x0002); System.Threading.Thread.Sleep(350);
    RECT r; GetWindowRect(h,out r);
    using(Bitmap bmp=new Bitmap(r.R-r.L,r.B-r.T)){ using(Graphics g=Graphics.FromImage(bmp)){ g.CopyFromScreen(r.L,r.T,0,0,new Size(r.R-r.L,r.B-r.T)); } bmp.Save(path,System.Drawing.Imaging.ImageFormat.Png);} return "saved";
  }
  public static string Wheel(int notches){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    int cx=r.L+(r.R-r.L)/2, cy=r.T+(r.B-r.T)/2;
    SetForegroundWindow(h); System.Threading.Thread.Sleep(200);
    SetCursorPos(cx,cy); System.Threading.Thread.Sleep(120);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(80);
    for(int i=0;i<Math.Abs(notches);i++){ mouse_event(0x0800,0,0,notches>0?120:-120,IntPtr.Zero); System.Threading.Thread.Sleep(60); }
    System.Threading.Thread.Sleep(450);
    return "wheel";
  }
}
"@ -ReferencedAssemblies System.Drawing

$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$env:NEOSONIA_PAGE = "settings"
$out = "E:\NeoSonia\logs\ui"

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 800
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 1500
for ($k = 1; $k -le 6; $k++) {
    [void][SHS]::Wheel(-6)
    [void][SHS]::Shot("$out\v4-settings-scroll-$k.png")
}
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item Env:NEOSONIA_PAGE -ErrorAction SilentlyContinue
Get-ChildItem $out\v4-settings-scroll-*.png | Select-Object Name,@{n='KB';e={[math]::Round($_.Length/1KB,0)}}
