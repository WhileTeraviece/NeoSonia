$ErrorActionPreference="Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class HKQ {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public InputUnion u; }
  [StructLayout(LayoutKind.Explicit)] public struct InputUnion { [FieldOffset(0)] public MOUSEINPUT mi; [FieldOffset(0)] public KEYBDINPUT ki; [FieldOffset(0)] public HARDWAREINPUT hi; }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT { public int dx; public int dy; public uint mouseData; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }
  [StructLayout(LayoutKind.Sequential)] public struct HARDWAREINPUT { public uint uMsg; public ushort wParamL; public ushort wParamH; }
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int cb);
  static void Down(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  static void Up(ushort vk){ INPUT[] a=new INPUT[1]; a[0].type=1; a[0].u.ki.wVk=vk; a[0].u.ki.dwFlags=2; SendInput(1,a,Marshal.SizeOf(typeof(INPUT))); }
  public static void AltSpace(){ Down(0xA4); System.Threading.Thread.Sleep(40); Down(0x20); System.Threading.Thread.Sleep(40); Up(0x20); System.Threading.Thread.Sleep(40); Up(0xA4); }
  public static void CtrlAltA(){ Down(0xA2); Down(0xA4); System.Threading.Thread.Sleep(40); Down(0x41); System.Threading.Thread.Sleep(40); Up(0x41); System.Threading.Thread.Sleep(40); Up(0xA4); Up(0xA2); }
  public static string Vis(){ SetProcessDPIAware(); IntPtr h=FindWindowW(null,"NeoSonia"); if(h==IntPtr.Zero) return "no-window"; return IsWindowVisible(h)?"visible":"hidden"; }
}
"@
$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
$env:NEOSONIA_HOTKEY_DEBUG = "1"
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 1500
"start=$([HKQ]::Vis())"
Remove-Item "E:\NeoSonia\sandbox\data\screenshots\shot-*.png" -ErrorAction SilentlyContinue
[HKQ]::AltSpace(); Start-Sleep -Seconds 1; "after1=$([HKQ]::Vis())"
"shots1=$((Get-ChildItem "E:\NeoSonia\sandbox\data\screenshots\shot-*.png" -ErrorAction SilentlyContinue).Count)"
# 隐藏后按截图快捷键（Ctrl+Alt+A）看热键通路是否还活着
[HKQ]::CtrlAltA(); Start-Sleep -Seconds 3
"hidden-then-screenshot shots=$((Get-ChildItem "E:\NeoSonia\sandbox\data\screenshots\shot-*.png" -ErrorAction SilentlyContinue).Count)"
Write-Host "=== log ==="
Get-Content "E:\NeoSonia\sandbox\logs\neosonia.log" -Tail 20
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item Env:NEOSONIA_HOTKEY_DEBUG -ErrorAction SilentlyContinue