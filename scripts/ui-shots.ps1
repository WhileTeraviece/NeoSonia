# 桌面浮窗各页面截图（真机，用于视觉验收）
# 用法: powershell -ExecutionPolicy Bypass -File scripts\ui-shots.ps1 [-Home E:\NeoSonia\sandbox] [-OutDir E:\NeoSonia\logs\ui]
param(
    [string]$NeoHome = "E:\NeoSonia\sandbox",
    [string]$OutDir = "E:\NeoSonia\logs\ui",
    [string]$Pages = "chat,settings,history,remote,memory,persona,mail"
)

$ErrorActionPreference = "Continue"
New-Item -ItemType Directory -Force $OutDir | Out-Null
$exe = "E:\NeoSonia\target\debug\neosonia.exe"

$code = @"
using System;
using System.Text;
using System.Drawing;
using System.Runtime.InteropServices;
public class Shot {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  public static IntPtr Find() { SetProcessDPIAware(); return FindWindowW(null, "NeoSonia"); }
  public static string Capture(string path) {
    IntPtr h = Find();
    if (h == IntPtr.Zero) return "no window";
    SetWindowPos(h, new IntPtr(-1), 0, 0, 0, 0, 0x0001 | 0x0002);
    System.Threading.Thread.Sleep(400);
    RECT r; GetWindowRect(h, out r);
    int w = r.R - r.L, hh = r.B - r.T;
    if (w <= 0 || hh <= 0) return "bad rect";
    using (Bitmap bmp = new Bitmap(w, hh)) {
      using (Graphics g = Graphics.FromImage(bmp)) { g.CopyFromScreen(r.L, r.T, 0, 0, new Size(w, hh)); }
      bmp.Save(path, System.Drawing.Imaging.ImageFormat.Png);
    }
    return "ok " + w + "x" + hh + " @(" + r.L + "," + r.T + ")";
  }
  public static string ClickRel(int rx, int ry) {
    IntPtr h = Find();
    if (h == IntPtr.Zero) return "no window";
    RECT r; GetWindowRect(h, out r);
    SetForegroundWindow(h);
    System.Threading.Thread.Sleep(300);
    int x = r.L + rx, y = r.T + ry;
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(150);
    mouse_event(2, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(90);
    mouse_event(4, 0, 0, 0, IntPtr.Zero);
    return "click " + x + "," + y;
  }
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
}
"@
Add-Type -TypeDefinition $code -ReferencedAssemblies System.Drawing,System.Windows.Forms

function Wait-Window([int]$timeoutSec = 40) {
    for ($i = 0; $i -lt ($timeoutSec * 4); $i++) {
        Start-Sleep -Milliseconds 250
        $h = [Shot]::Find()
        if ($h -ne [IntPtr]::Zero) { Start-Sleep -Milliseconds 1200; return $true }
    }
    return $false
}

foreach ($page in ($Pages -split ",")) {
    $page = $page.Trim()
    if (-not $page) { continue }
    Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 600
    Remove-Item (Join-Path $NeoHome "logs\neosonia.log") -ErrorAction SilentlyContinue
    $env:NEOSONIA_PAGE = if ($page -eq "chat") { "" } else { $page }
    $proc = Start-Process -FilePath $exe -ArgumentList "--home", $NeoHome -PassThru
    if (-not (Wait-Window)) {
        Write-Host "[$page] 窗口没出现（进程退出=$($proc.HasExited)）"
        continue
    }
    # 等首帧 + 自动停靠完成（日志里出现 dock attempt），否则窗口还是空白/未定位
    $logPath = Join-Path $NeoHome "logs\neosonia.log"
    for ($i = 0; $i -lt 120; $i++) {
        Start-Sleep -Milliseconds 300
        if ((Test-Path $logPath) -and (Select-String -Path $logPath -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break }
    }
    Start-Sleep -Milliseconds 1200
    $out = Join-Path $OutDir "ui-$page.png"
    $res = [Shot]::Capture($out)
    Write-Host "[$page] $res -> $out"
    if (-not $proc.HasExited) { $proc.Kill() }
    Start-Sleep -Milliseconds 500
}
Remove-Item Env:NEOSONIA_PAGE -ErrorAction SilentlyContinue
Write-Host "done"
