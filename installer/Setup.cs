// NeoSonia 自解压安装程序（C# / .NET Framework 4.x，单文件内嵌 payload.zip）
//
// 用法：
//   NeoSoniaSetup-0.9.0-win64.exe            交互式安装（提示目录/完成）
//   NeoSoniaSetup-0.9.0-win64.exe /S         静默安装
//   NeoSoniaSetup-0.9.0-win64.exe /D=D:\x    指定安装目录（配合 /S）
//
// 行为：
//   - 解压内嵌 payload.zip（neosonia.exe + assets + docs + Uninstall.exe）到安装目录；
//   - 创建 config/data/logs/workspace/personas/skills 目录（数据跟随安装位置）；
//   - 创建开始菜单快捷方式（带 --home 指向安装目录）；
//   - 写 HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\NeoSonia
//     （Windows「应用和功能」里可见，含 DisplayName/Version/Publisher/UninstallString）；
//   - 不删旧 config/data：覆盖安装即升级，保留用户数据。

using System;
using System.Diagnostics;
using System.Drawing;
using System.IO;
using System.IO.Compression;
using System.Reflection;
using System.Security.Principal;
using System.Windows.Forms;
using Microsoft.Win32;

internal static class Setup
{
    private const string Product = "NeoSonia";
    private static string Version = "0.9.0";
    private const string Publisher = "NeoSonia";
    private const string PayloadResource = "payload.zip";

    [STAThread]
    private static int Main(string[] args)
    {
        bool silent = HasArg(args, "/S") || HasArg(args, "/silent");
        string dir = ParseDir(args);
        bool explicitDir = ParseDir(args) != null;
        if (dir == null)
        {
            dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                               "Programs", Product);
        }

        if (!silent)
        {
            // 没有 /D= 时弹出安装位置选择（可浏览目录）；/D= 或 /S 直接用指定/默认位置。
            if (!explicitDir)
            {
                var picked = PickInstallDir(dir);
                if (picked == null) return 2;
                dir = picked;
            }
            var msg = "即将安装 " + Product + " v" + Version + "\n\n安装位置：\n" + dir +
                      "\n\n配置、聊天记录、workspace、skills 都会保存在安装目录下。\n开始安装？";
            var r = System.Windows.Forms.MessageBox.Show(msg, Product + " 安装程序",
                System.Windows.Forms.MessageBoxButtons.OKCancel,
                System.Windows.Forms.MessageBoxIcon.Information);
            if (r != System.Windows.Forms.DialogResult.OK) return 2;
        }

        try
        {
            KillRunning();

            Directory.CreateDirectory(dir);
            ExtractPayload(dir);
        RemoveLegacyUninstaller(dir);
            Version = ReadVersionFromPayload(dir) ?? Version;

            // 数据目录骨架（应用启动时也会 ensure，这里先建好让用户直观看到）
            foreach (var sub in new[] { "config", "data", "logs", "workspace", "personas", "skills" })
                Directory.CreateDirectory(Path.Combine(dir, sub));

            CreateShortcuts(dir);
            WriteUninstallEntry(dir);

            if (!silent)
            {
                var ask = System.Windows.Forms.MessageBox.Show(
                    "安装完成。\n\n是否立即启动 " + Product + "？", Product,
                    System.Windows.Forms.MessageBoxButtons.YesNo,
                    System.Windows.Forms.MessageBoxIcon.Question);
                if (ask == System.Windows.Forms.DialogResult.Yes)
                {
                    Process.Start(new ProcessStartInfo
                    {
                        FileName = Path.Combine(dir, "neosonia.exe"),
                        Arguments = "--home \"" + dir + "\"",
                        UseShellExecute = true,
                        WorkingDirectory = dir,
                    });
                }
            }
            return 0;
        }
        catch (Exception ex)
        {
            if (!silent)
                System.Windows.Forms.MessageBox.Show("安装失败：" + ex.Message, Product,
                    System.Windows.Forms.MessageBoxButtons.OK,
                    System.Windows.Forms.MessageBoxIcon.Error);
            return 1;
        }
    }

    /// 从 payload 的 VERSION.txt 读版本（"NeoSonia 0.9.0 - ..."），保证与打包版本一致。
    private static string ReadVersionFromPayload(string dir)
    {
        try
        {
            var vf = Path.Combine(dir, "VERSION.txt");
            if (!File.Exists(vf)) return null;
            var line = File.ReadAllLines(vf)[0];
            var m = System.Text.RegularExpressions.Regex.Match(line, @"^NeoSonia\s+([0-9][0-9A-Za-z\.\-]*)");
            return m.Success ? m.Groups[1].Value : null;
        }
        catch { return null; }
    }

    /// 交互式安装：弹出安装位置选择窗口（文本框 + 浏览目录 + 安装/取消）。
    /// 返回 null 表示用户取消。
    private static string PickInstallDir(string defaultDir)
    {
        using (var form = new Form())
        {
            form.Text = Product + " 安装程序";
            form.FormBorderStyle = FormBorderStyle.FixedDialog;
            form.StartPosition = FormStartPosition.CenterScreen;
            form.MaximizeBox = false;
            form.MinimizeBox = false;
            form.ClientSize = new Size(600, 200);
            try { form.Font = new Font("Microsoft YaHei UI", 9F); } catch { }

            var title = new Label
            {
                Text = "安装 " + Product + " v" + Version,
                Location = new Point(16, 14),
                AutoSize = true,
                Font = new Font(form.Font.FontFamily, 12F, FontStyle.Bold),
            };
            var lblDir = new Label { Text = "安装位置：", Location = new Point(16, 56), AutoSize = true };
            var txtDir = new TextBox { Text = defaultDir, Location = new Point(16, 78), Width = 470 };
            var btnBrowse = new Button { Text = "浏览…", Location = new Point(496, 76), Width = 86 };
            var lblHint = new Label
            {
                Text = "配置、聊天记录、workspace、personas、skills、logs 都会保存在该目录下。",
                Location = new Point(16, 112),
                AutoSize = true,
                ForeColor = Color.Gray,
            };
            var btnOk = new Button { Text = "安装", DialogResult = DialogResult.OK, Location = new Point(400, 152), Width = 88 };
            var btnCancel = new Button { Text = "取消", DialogResult = DialogResult.Cancel, Location = new Point(496, 152), Width = 86 };

            btnBrowse.Click += delegate
            {
                // 注意：主窗口不能 TopMost，否则文件夹选择对话框会被压在"最顶层"窗口后面，
                // 用户看到的是"安装程序（未响应）"。
                form.TopMost = false;
                using (var dlg = new FolderBrowserDialog())
                {
                    dlg.Description = "选择 " + Product + " 的安装位置";
                    dlg.ShowNewFolderButton = true;
                    try { dlg.SelectedPath = txtDir.Text; } catch { }
                    var result = dlg.ShowDialog();
                    form.Activate();
                    form.BringToFront();
                    if (result == DialogResult.OK)
                    {
                        var chosen = dlg.SelectedPath.TrimEnd('\\');
                        // 若用户直接选了目标父目录，自动追加产品名，避免装进桌面/盘符根。
                        if (string.Equals(Path.GetFileName(chosen), Product, StringComparison.OrdinalIgnoreCase))
                            txtDir.Text = chosen;
                        else
                            txtDir.Text = Path.Combine(chosen, Product);
                    }
                }
            };

            form.AcceptButton = btnOk;
            form.CancelButton = btnCancel;
            form.Controls.AddRange(new Control[] { title, lblDir, txtDir, btnBrowse, lblHint, btnOk, btnCancel });

            if (form.ShowDialog() != DialogResult.OK) return null;
            var v = txtDir.Text.Trim().Trim('"');
            return v.Length == 0 ? null : v;
        }
    }

    private static bool HasArg(string[] args, string name)
    {
        foreach (var a in args)
            if (string.Equals(a, name, StringComparison.OrdinalIgnoreCase)) return true;
        return false;
    }

    private static string ParseDir(string[] args)
    {
        foreach (var a in args)
        {
            if (a.StartsWith("/D=", StringComparison.OrdinalIgnoreCase))
            {
                var v = a.Substring(3).Trim().Trim('"');
                return string.IsNullOrEmpty(v) ? null : v;
            }
        }
        return null;
    }

    private static void KillRunning()
    {
        foreach (var p in Process.GetProcessesByName("neosonia"))
        {
            try { p.Kill(); p.WaitForExit(3000); } catch { }
        }
    }

    private static void ExtractPayload(string dir)
    {
        var asm = Assembly.GetExecutingAssembly();
        using (var s = asm.GetManifestResourceStream(PayloadResource))
        {
            if (s == null) throw new InvalidOperationException("安装包损坏：找不到 payload.zip");
            using (var zip = new ZipArchive(s, ZipArchiveMode.Read))
            {
                foreach (var e in zip.Entries)
                {
                    var target = Path.Combine(dir, e.FullName.Replace('/', Path.DirectorySeparatorChar));
                    if (e.FullName.EndsWith("/")) { Directory.CreateDirectory(target); continue; }
                    Directory.CreateDirectory(Path.GetDirectoryName(target));
                    try
                    {
                        using (var input = e.Open())
                        using (var output = new FileStream(target, FileMode.Create, FileAccess.Write, FileShare.None))
                            input.CopyTo(output);
                    }
                    catch (IOException)
                    {
                        // 正在运行的 exe 等文件被占用：跳过（KillRunning 已尽力）。
                    }
                }
            }
        }
    }

    /// 删除旧版本残留的 Uninstall.exe；新版本卸载走 neosonia.exe --uninstall。
    private static void RemoveLegacyUninstaller(string dir)
    {
        try { File.Delete(Path.Combine(dir, "Uninstall.exe")); } catch { }
    }

    private static void CreateShortcuts(string dir)
    {
        var exe = Path.Combine(dir, "neosonia.exe");
        var programs = Environment.GetFolderPath(Environment.SpecialFolder.Programs);
        var menuDir = Path.Combine(programs, Product);
        Directory.CreateDirectory(menuDir);
        CreateLnk(Path.Combine(menuDir, Product + ".lnk"), exe, "--home \"" + dir + "\"", dir, dir + "\\neosonia.exe");
        // 卸载不再依赖独立 Uninstall.exe（安全软件会拦截/隔离自删除卸载器）：
        // 指向主程序的 --uninstall 模式，删除目录由主程序交给分离的 cmd 完成。
        CreateLnk(Path.Combine(menuDir, "卸载 " + Product + ".lnk"),
                  Path.Combine(dir, "neosonia.exe"), "--uninstall", dir, exe);

        // 桌面快捷方式（失败不致命）
        try
        {
            var desktop = Environment.GetFolderPath(Environment.SpecialFolder.DesktopDirectory);
            CreateLnk(Path.Combine(desktop, Product + ".lnk"), exe, "--home \"" + dir + "\"", dir, exe);
        }
        catch { }
    }

    private static void CreateLnk(string lnkPath, string target, string args, string workDir, string icon)
    {
        var shellType = Type.GetTypeFromProgID("WScript.Shell");
        if (shellType == null) return;
        var shell = Activator.CreateInstance(shellType);
        var shortcut = shellType.InvokeMember("CreateShortcut", BindingFlags.InvokeMethod, null, shell,
                                              new object[] { lnkPath });
        var st = shortcut.GetType();
        st.InvokeMember("TargetPath", BindingFlags.SetProperty, null, shortcut, new object[] { target });
        st.InvokeMember("Arguments", BindingFlags.SetProperty, null, shortcut, new object[] { args });
        st.InvokeMember("WorkingDirectory", BindingFlags.SetProperty, null, shortcut, new object[] { workDir });
        if (File.Exists(icon))
            st.InvokeMember("IconLocation", BindingFlags.SetProperty, null, shortcut, new object[] { icon + ",0" });
        st.InvokeMember("Save", BindingFlags.InvokeMethod, null, shortcut, null);
    }

    private static void WriteUninstallEntry(string dir)
    {
        var sizeKb = 0;
        try
        {
            foreach (var f in Directory.GetFiles(dir, "*", SearchOption.AllDirectories))
                sizeKb += (int)(new FileInfo(f).Length / 1024);
        }
        catch { }

        using (var key = Registry.CurrentUser.CreateSubKey(
            @"Software\Microsoft\Windows\CurrentVersion\Uninstall\" + Product))
        {
            if (key == null) throw new InvalidOperationException("无法写入卸载注册表项");
            key.SetValue("DisplayName", Product + " v" + Version);
            key.SetValue("DisplayVersion", Version);
            key.SetValue("Publisher", Publisher);
            key.SetValue("InstallLocation", dir);
            key.SetValue("DisplayIcon", Path.Combine(dir, "neosonia.exe") + ",0");
            key.SetValue("UninstallString", "\"" + Path.Combine(dir, "neosonia.exe") + "\" --uninstall");
            key.SetValue("QuietUninstallString", "\"" + Path.Combine(dir, "neosonia.exe") + "\" --uninstall --silent");
            key.SetValue("InstallDate", DateTime.Now.ToString("yyyyMMdd"));
            key.SetValue("EstimatedSize", sizeKb, RegistryValueKind.DWord);
            key.SetValue("NoModify", 1, RegistryValueKind.DWord);
            key.SetValue("NoRepair", 1, RegistryValueKind.DWord);
        }
    }
}
