// NeoSonia 卸载程序（C# / .NET Framework 4.x）
//
// 用法：
//   Uninstall.exe         交互式卸载（确认 + 完成提示）
//   Uninstall.exe /S      静默卸载
//
// 行为：
//   - 结束正在运行的 NeoSonia；
//   - 删除开始菜单/桌面快捷方式；
//   - 删除 HKCU Uninstall 注册表项（「应用和功能」条目消失）；
//   - 递归删除安装目录（配置、聊天记录、workspace、skills 都在安装目录内，一并删除）；
//   - 由于 Windows 不能删除正在运行的自身，先把自己复制到 %TEMP% 再删除原目录。

using System;
using System.Diagnostics;
using System.IO;
using System.Windows.Forms;
using Microsoft.Win32;

internal static class Uninstall
{
    private const string Product = "NeoSonia";

    [STAThread]
    private static int Main(string[] args)
    {
        bool silent = false;
        foreach (var a in args)
            if (string.Equals(a, "/S", StringComparison.OrdinalIgnoreCase) ||
                string.Equals(a, "/silent", StringComparison.OrdinalIgnoreCase)) silent = true;

        // 第二阶段：从 %TEMP% 里运行的副本，负责删除安装目录
        bool deletePhase = false;
        string installDir = null;
        foreach (var a in args)
            if (a.StartsWith("/DEL=", StringComparison.OrdinalIgnoreCase)) { deletePhase = true; installDir = a.Substring(5).Trim('"'); }

        if (deletePhase)
        {
            KillRunning();
            // 原 Uninstall.exe 可能还在退出中：重试删除，直到目录消失或超时。
            for (int i = 0; i < 30; i++)
            {
                try
                {
                    if (!Directory.Exists(installDir)) break;
                    Directory.Delete(installDir, true);
                }
                catch { }
                if (!Directory.Exists(installDir)) break;
                System.Threading.Thread.Sleep(500);
            }
            // 清理 %TEMP% 里的自身副本
            try
            {
                Process.Start(new ProcessStartInfo
                {
                    FileName = "cmd.exe",
                    Arguments = "/c timeout /t 1 >nul & del \"" + Process.GetCurrentProcess().MainModule.FileName + "\"",
                    UseShellExecute = false,
                    CreateNoWindow = true,
                    WindowStyle = ProcessWindowStyle.Hidden,
                });
            }
            catch { }
            return 0;
        }

        installDir = AppDomain.CurrentDomain.BaseDirectory.TrimEnd('\\');

        if (!silent)
        {
            var r = MessageBox.Show("卸载 " + Product + "？\n\n将删除安装目录及其中的配置、聊天记录、workspace、skills：\n" + installDir,
                Product + " 卸载程序", MessageBoxButtons.YesNo, MessageBoxIcon.Warning);
            if (r != DialogResult.Yes) return 2;
        }

        try
        {
            RemoveShortcuts();
            RemoveUninstallEntry();
        }
        catch { }

        // 复制自己到 %TEMP%，由副本删除安装目录（含本 exe）
        try
        {
            var temp = Path.Combine(Path.GetTempPath(), Product + "-uninstall-" + Guid.NewGuid().ToString("N") + ".exe");
            File.Copy(Process.GetCurrentProcess().MainModule.FileName, temp, true);
            Process.Start(new ProcessStartInfo
            {
                FileName = temp,
                Arguments = "/DEL=\"" + installDir + "\"",
                UseShellExecute = true,
                WindowStyle = ProcessWindowStyle.Hidden,
            });
            // 给副本一点时间释放自身，然后退出
            System.Threading.Thread.Sleep(300);
        }
        catch { }

        if (!silent)
        {
            MessageBox.Show("已卸载 " + Product + "。", Product, MessageBoxButtons.OK, MessageBoxIcon.Information);
        }
        return 0;
    }

    private static void KillRunning()
    {
        foreach (var p in Process.GetProcessesByName("neosonia"))
        {
            try { p.Kill(); p.WaitForExit(3000); } catch { }
        }
    }

    private static void RemoveShortcuts()
    {
        var programs = Environment.GetFolderPath(Environment.SpecialFolder.Programs);
        var menuDir = Path.Combine(programs, Product);
        try { if (Directory.Exists(menuDir)) Directory.Delete(menuDir, true); } catch { }
        try
        {
            var desktop = Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory);
            var lnk = Path.Combine(desktop, Product + ".lnk");
            if (File.Exists(lnk)) File.Delete(lnk);
        }
        catch { }
    }

    private static void RemoveUninstallEntry()
    {
        Registry.CurrentUser.DeleteSubKeyTree(
            @"Software\Microsoft\Windows\CurrentVersion\Uninstall\" + Product, false);
    }
}
