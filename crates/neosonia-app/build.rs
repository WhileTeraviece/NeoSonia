// NeoSonia: 把应用图标 + 版本信息编进 exe（资源管理器/任务栏/文件属性显示）。
//
// 之前 exe 没有图标资源：窗口/托盘图标是运行时用 PNG 设置的，但文件管理器和
// 任务栏读的是 PE 资源里的 ICON，所以显示默认空白图标。
//
// 用 embed-resource 在 Windows 上编译 res/app.rc（ICON + VERSIONINFO）；
// 非 Windows 目标跳过。

fn main() {
    println!("cargo:rerun-if-changed=res/app.rc");
    println!("cargo:rerun-if-changed=res/neosonia.ico");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let version_comma = version.replace('.', ",") + ",0";
    let icon = std::fs::canonicalize("res/neosonia.ico").expect("res/neosonia.ico");
    let icon = icon.to_string_lossy().replace('\\', "/");

    let rc = format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
 FILEVERSION {version_comma}
 PRODUCTVERSION {version_comma}
 FILEOS 0x40004
 FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "NeoSonia"
      VALUE "FileDescription", "NeoSonia 桌面 AI Agent"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "neosonia"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "neosonia.exe"
      VALUE "ProductName", "NeoSonia"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );

    let rc_path = std::path::Path::new(&out_dir).join("neosonia.rc");
    std::fs::write(&rc_path, rc).expect("write rc");
    embed_resource::compile(&rc_path, embed_resource::NONE);
}
