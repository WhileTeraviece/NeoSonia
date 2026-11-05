$ErrorActionPreference = "Continue"
Set-Location "E:\NeoSonia"
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public class HKV5 {
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
  public static string Vis(){ SetProcessDPIAware(); IntPtr h=FindWindowW(null,"NeoSonia"); if(h==IntPtr.Zero) return "no-window"; return IsWindowVisible(h)?"visible":"hidden"; }
  public static int InputSize(){ return Marshal.SizeOf(typeof(INPUT)); }
}
"@
$cfg2 = Get-Content E:\Sonia\config\settings.yaml -Raw
$env:NEOSONIA_API_KEY = [regex]::Match($cfg2,'api_key:\s*(\S+)').Groups[1].Value
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700
Remove-Item "E:\NeoSonia\sandbox\logs\neosonia.log" -ErrorAction SilentlyContinue
$p = Start-Process -FilePath "E:\NeoSonia\target\debug\neosonia.exe" -ArgumentList "--home","E:\NeoSonia\sandbox" -PassThru
for ($i = 0; $i -lt 300; $i++) { Start-Sleep -Milliseconds 300; if ((Test-Path "E:\NeoSonia\sandbox\logs\neosonia.log") -and (Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break } }
Start-Sleep -Milliseconds 1500
"INPUT size = $([HKV5]::InputSize())"
"start: $([HKV5]::Vis())"
for ($k = 1; $k -le 4; $k++) {
    [HKV5]::AltSpace()
    $t0 = Get-Date
    while (((Get-Date) - $t0).TotalMilliseconds -lt 1500) {
        $v = [HKV5]::Vis()
        if ($k % 2 -eq 1 -and $v -eq "hidden") { break }
        if ($k % 2 -eq 0 -and $v -eq "visible") { break }
        Start-Sleep -Milliseconds 5
    }
    "$k -> $([HKV5]::Vis()) after $([int]((Get-Date) - $t0).TotalMilliseconds)ms"
    Start-Sleep -Milliseconds 300
}
Write-Host "=== log ==="
Select-String -Path "E:\NeoSonia\sandbox\logs\neosonia.log" -Pattern "hotkey|快捷键线程" | ForEach-Object { $_.Line } | Select-Object -Last 10
Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
