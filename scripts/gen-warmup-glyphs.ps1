# 从 app/core 源码抽取界面用到的 CJK 字符，生成 warmup-glyphs.txt（字形预热用）
$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")
$files = Get-ChildItem crates\neosonia-app\src\*.rs, crates\neosonia-app\src\panels\*.rs, crates\neosonia-core\src\*.rs -File
$chars = New-Object System.Collections.Generic.HashSet[string]
foreach ($f in $files) {
    $t = Get-Content $f.FullName -Raw -Encoding utf8
    foreach ($ch in $t.ToCharArray()) {
        $code = [int][char]$ch
        if (($code -ge 0x4E00 -and $code -le 0x9FFF) -or ($code -ge 0x3000 -and $code -le 0x303F) -or ($code -ge 0xFF00 -and $code -le 0xFFEF)) { [void]$chars.Add([string]$ch) }
    }
}
$glyphs = -join ($chars | Sort-Object)
$sb = New-Object System.Text.StringBuilder
for ($i = 0; $i -lt $glyphs.Length; $i += 48) {
    $len = [Math]::Min(48, $glyphs.Length - $i)
    [void]$sb.AppendLine($glyphs.Substring($i, $len))
}
$out = "crates\neosonia-app\src\warmup-glyphs.txt"
Set-Content -Path $out -Value $sb.ToString() -Encoding utf8 -NoNewline
Write-Host "glyphs=$($glyphs.Length) -> $out"