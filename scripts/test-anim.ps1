$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class A6 {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int dx, int dy, int d, IntPtr e);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  static IntPtr Find(){ SetProcessDPIAware(); return FindWindowW(null,"NeoSonia"); }
  public static string Click(int rx, int ry){
    IntPtr h=Find(); if(h==IntPtr.Zero) return "no-window";
    RECT r; GetWindowRect(h,out r);
    SetForegroundWindow(h); System.Threading.Thread.Sleep(120);
    SetCursorPos(r.L+rx, r.T+ry); System.Threading.Thread.Sleep(70);
    mouse_event(0x0001,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(25);
    mouse_event(0x0002,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(25);
    mouse_event(0x0004,0,0,0,IntPtr.Zero); System.Threading.Thread.Sleep(150);
    return "ok";
  }
}
"@
$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$env:NEOSONIA_FRAME_LOG = "1"
$env:NEOSONIA_PROFILE = "1"
Get-Process neosonia -ErrorAction SilentlyContinue | Where-Object { $_.Id -ne 30476 } | Stop-Process -Force
Start-Sleep -Milliseconds 500
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
Remove-Item "E:\NeoSonia\logs\anim-check.err" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru -RedirectStandardError "E:\NeoSonia\logs\anim-check.err"
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 2000
# 切几次页
foreach ($x in @(62,106,157,201,19)) { [void][A6]::Click([int]($x*2.5), [int](68*2.5)); Start-Sleep -Milliseconds 600 }
Start-Sleep -Seconds 3
$before = (Get-Item "E:\NeoSonia\logs\anim-check.err").Length
Start-Sleep -Seconds 3
$after = (Get-Item "E:\NeoSonia\logs\anim-check.err").Length
"idle 3s stderr bytes: $before -> $after (delta=$($after-$before))"
Write-Host "=== 最近帧日志（idle 期间应为空） ==="
Get-Content "E:\NeoSonia\logs\anim-check.err" -Tail 6
Get-Process neosonia -ErrorAction SilentlyContinue | Where-Object { $_.Id -eq $p.Id } | Stop-Process -Force
Remove-Item Env:NEOSONIA_FRAME_LOG -ErrorAction SilentlyContinue
Remove-Item Env:NEOSONIA_PROFILE -ErrorAction SilentlyContinue
