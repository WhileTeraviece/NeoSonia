# 抓取 NeoSonia 浮窗的截图（用于开发期视觉验证）
# 用法: powershell -ExecutionPolicy Bypass -File scripts\capture.ps1 -Out shot.png [-WaitSeconds 8]
param(
    [string]$Out = "screenshot.png",
    [int]$WaitSeconds = 8,
    [string]$Exe = "",
    [switch]$NoLaunch
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

if (-not $NoLaunch) {
    if ([string]::IsNullOrEmpty($Exe)) { $Exe = "E:\NeoSonia\target\debug\neosonia.exe" }
    if (-not (Test-Path $Exe)) { Write-Error "找不到 exe: $Exe"; exit 1 }
    $proc = Start-Process -FilePath $Exe -PassThru
    Start-Sleep -Seconds $WaitSeconds
}

$bounds = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bmp = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
$gfx = [System.Drawing.Graphics]::FromImage($bmp)
$gfx.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
$dir = Split-Path -Parent $Out
if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$gfx.Dispose()
$bmp.Dispose()
Write-Output "saved: $Out ($($bounds.Width)x$($bounds.Height))"
if ($proc) { Write-Output "pid: $($proc.Id)" }
