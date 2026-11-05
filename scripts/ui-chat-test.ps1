# 在桌面浮窗里真实输入一条消息并截图（验证 markdown/公式/流式渲染/工具卡片）
# 用法: powershell -ExecutionPolicy Bypass -File scripts\ui-chat-test.ps1 [-Home E:\NeoSonia\sandbox]
param(
    [string]$NeoHome = "E:\NeoSonia\sandbox",
    [string]$Text = "请用一句话介绍你自己，然后用公式 `$E=mc^2` 和 `$\frac{a}{b}$` 各举一个例子。",
    [int]$WaitSeconds = 30,
    [string]$OutDir = "E:\NeoSonia\logs\ui"
)

$ErrorActionPreference = "Continue"
New-Item -ItemType Directory -Force $OutDir | Out-Null
$exe = "E:\NeoSonia\target\debug\neosonia.exe"

$code = @"
using System;
using System.Text;
using System.Drawing;
using System.Runtime.InteropServices;
public class ChatBot {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string c, string t);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool f);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int cb);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L,T,R,B; }
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public U u; }
  [StructLayout(LayoutKind.Explicit)] public struct U { [FieldOffset(0)] public KEYBDINPUT ki; }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort wVk; public ushort wScan; public uint dwFlags; public uint time; public IntPtr dwExtraInfo; }

  static IntPtr Find() { SetProcessDPIAware(); return FindWindowW(null, "NeoSonia"); }

  public static string FocusForce() {
    IntPtr h = Find();
    if (h == IntPtr.Zero) return "no window";
    SetWindowPos(h, new IntPtr(-1), 0, 0, 0, 0, 0x0001 | 0x0002);
    System.Threading.Thread.Sleep(200);
    IntPtr fg = GetForegroundWindow();
    uint fgPid = 0, myPid = 0;
    uint fgTid = GetWindowThreadProcessId(fg, out fgPid);
    uint myTid = GetWindowThreadProcessId(h, out myPid);
    uint cur = GetCurrentThreadId();
    // 经典强制前台：把自己和前台线程输入队列附加，再 SetForegroundWindow
    AttachThreadInput(cur, fgTid, true);
    AttachThreadInput(cur, myTid, true);
    BringWindowToTop(h);
    bool ok = SetForegroundWindow(h);
    AttachThreadInput(cur, fgTid, false);
    AttachThreadInput(cur, myTid, false);
    System.Threading.Thread.Sleep(500);
    return "focus=" + ok + " fg=" + GetForegroundWindow() + " win=" + h;
  }

  public static string ClickComposer() {
    IntPtr h = Find();
    RECT r; GetWindowRect(h, out r);
    int x = r.L + (r.R - r.L) / 2;
    int y = r.B - 120;
    SetCursorPos(x, y);
    System.Threading.Thread.Sleep(200);
    mouse_event(2, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(120);
    mouse_event(4, 0, 0, 0, IntPtr.Zero);
    System.Threading.Thread.Sleep(400);
    return "clicked " + x + "," + y;
  }

  public static string TypeText(string s) {
    foreach (char ch in s) {
      INPUT[] a = new INPUT[2];
      a[0].type = 1; a[0].u.ki.wScan = (ushort)ch; a[0].u.ki.wFlags_not_used();
      a[1].type = 1; a[1].u.ki.wScan = (ushort)ch;
      SendInput(2, a, Marshal.SizeOf(typeof(INPUT)));
      System.Threading.Thread.Sleep(18);
    }
    return "typed";
  }

  public static string Key(ushort vk) {
    INPUT[] a = new INPUT[1]; a[0].type = 1; a[0].u.ki.wVk = vk;
    SendInput(1, a, Marshal.SizeOf(typeof(INPUT)));
    System.Threading.Thread.Sleep(50);
    INPUT[] b = new INPUT[1]; b[0].type = 1; b[0].u.ki.wVk = vk; b[0].u.ki.dwFlags = 2;
    SendInput(1, b, Marshal.SizeOf(typeof(INPUT)));
    return "key";
  }

  public static string Capture(string path) {
    IntPtr h = Find();
    if (h == IntPtr.Zero) return "no window";
    SetWindowPos(h, new IntPtr(-1), 0, 0, 0, 0, 0x0001 | 0x0002);
    System.Threading.Thread.Sleep(400);
    RECT r; GetWindowRect(h, out r);
    using (Bitmap bmp = new Bitmap(r.R - r.L, r.B - r.T)) {
      using (Graphics g = Graphics.FromImage(bmp)) { g.CopyFromScreen(r.L, r.T, 0, 0, new Size(r.R - r.L, r.B - r.T)); }
      bmp.Save(path, System.Drawing.Imaging.ImageFormat.Png);
    }
    return "saved";
  }
}
"@
# 简化 TypeText 里的类型错误：重新定义（SendInput 的 KEYEVENTF_UNICODE = 0x4，抬起 = 0x6）
$code = $code -replace 'a\[0\]\.u\.ki\.wFlags_not_used\(\);', 'a[0].u.ki.dwFlags = 4;'
$code = $code -replace 'a\[1\]\.type = 1; a\[1\]\.u\.ki\.wScan = \(ushort\)ch;', 'a[1].type = 1; a[1].u.ki.wScan = (ushort)ch; a[1].u.ki.dwFlags = 6;'
Add-Type -TypeDefinition $code -ReferencedAssemblies System.Drawing,System.Windows.Forms

Get-Process neosonia -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 600
Remove-Item (Join-Path $NeoHome "logs\neosonia.log") -ErrorAction SilentlyContinue
$proc = Start-Process -FilePath $exe -ArgumentList "--home", $NeoHome -PassThru
for ($i = 0; $i -lt 160; $i++) {
    Start-Sleep -Milliseconds 250
    if ((Test-Path (Join-Path $NeoHome "logs\neosonia.log")) -and (Select-String -Path (Join-Path $NeoHome "logs\neosonia.log") -Pattern "dock attempt" -Quiet -ErrorAction SilentlyContinue)) { break }
}
Start-Sleep -Milliseconds 1500
Write-Host ("focus: " + [ChatBot]::FocusForce())
Write-Host ("click: " + [ChatBot]::ClickComposer())
Write-Host ("type: " + [ChatBot]::TypeText($Text))
Start-Sleep -Milliseconds 500
Write-Host ("enter: " + [ChatBot]::Key(0x0D))
Start-Sleep -Seconds $WaitSeconds
$out = Join-Path $OutDir "ui-chat-reply.png"
Write-Host ("cap: " + [ChatBot]::Capture($out))
if (-not $proc.HasExited) { }
