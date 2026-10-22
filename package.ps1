[CmdletBinding()]
param(
    # Export an already-built installer without rebuilding.
    [switch]$StageExisting
)

$ErrorActionPreference = 'Stop'
$projectRoot = $PSScriptRoot
$desktopDirectory = Join-Path $projectRoot 'crates\desktop'
$releaseDirectory = Join-Path $projectRoot 'release'
$bundleDirectory = Join-Path $projectRoot 'target\release\bundle\nsis'
$config = Get-Content -LiteralPath (Join-Path $desktopDirectory 'tauri.conf.json') -Raw |
    ConvertFrom-Json

if (-not $StageExisting) {
    if (-not $env:TAURI_SIGNING_PRIVATE_KEY) {
        $signingKeyPath = Join-Path $env:USERPROFILE '.tauri\crypto-news.key'
        if (Test-Path -LiteralPath $signingKeyPath) { $env:TAURI_SIGNING_PRIVATE_KEY = $signingKeyPath; $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = '' }
    }
    $cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
    $cargoPath = if ($cargoCommand) {
        $cargoCommand.Source
    } else {
        Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    }
    if (-not (Test-Path -LiteralPath $cargoPath)) {
        throw '找不到 cargo，请先安装 Rust 和 cargo-tauri。'
    }
    $savedPath = $env:PATH
    Push-Location $desktopDirectory
    try {
        $env:PATH = "$(Split-Path -Parent $cargoPath);$savedPath"
        & $cargoPath tauri build
        if ($LASTEXITCODE -ne 0) {
            throw "打包失败（退出码 $LASTEXITCODE），release 中的已有安装包未更新。"
        }
    } finally {
        Pop-Location
        $env:PATH = $savedPath
    }
}

$pattern = "$($config.productName)_$($config.version)_*-setup.exe"
$installer = Get-ChildItem -LiteralPath $bundleDirectory -Filter $pattern -File |
    Sort-Object LastWriteTimeUtc -Descending |
    Select-Object -First 1
if (-not $installer) {
    throw "未找到当前版本的安装包：$bundleDirectory\$pattern"
}

New-Item -ItemType Directory -Force -Path $releaseDirectory | Out-Null
# Build workspaces can inherit Low integrity. An installer launched with that
# label cannot write to the user's normal TEMP. Normalize only delivery output.
& icacls.exe $releaseDirectory /setintegritylevel '(OI)(CI)M' | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw '无法设置 release 的正常用户完整性级别，未导出安装包。'
}

$outputName = "CryptoNews_$($config.version)_x64-setup.exe"
$destination = Join-Path $releaseDirectory $outputName
Copy-Item -LiteralPath $installer.FullName -Destination $destination -Force
& icacls.exe $destination /setintegritylevel M | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw '安装包已复制，但完整性级别设置失败，请勿分发。'
}
$sourceHash = (Get-FileHash -LiteralPath $installer.FullName -Algorithm SHA256).Hash
$signaturePath = "$($installer.FullName).sig"
if (-not (Test-Path -LiteralPath $signaturePath)) { throw '缺少更新签名，未生成发行清单。' }
$signature = (Get-Content -LiteralPath $signaturePath -Raw).Trim()
if (-not $signature) { throw '更新签名为空。' }
Copy-Item -LiteralPath $signaturePath -Destination "$destination.sig" -Force
$outputHash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
if ($sourceHash -ne $outputHash) {
    throw '导出后的安装包 SHA-256 校验不一致，请勿分发。'
}
"$($outputHash.ToLowerInvariant())  $outputName" |
    Set-Content -LiteralPath (Join-Path $releaseDirectory "$outputName.sha256") -Encoding utf8
# Only prune old installers after the new output has passed hash verification.
Get-ChildItem -LiteralPath $releaseDirectory -Filter '*-setup.exe' -File |
    Where-Object { $_.Name -like "$($config.productName)_*-setup.exe" -or $_.Name -like 'CryptoNews_*-setup.exe' } |
    Where-Object { $_.FullName -ne $destination } |
    ForEach-Object {
        $oldPath = [IO.Path]::GetFullPath($_.FullName)
        $releaseRoot = [IO.Path]::GetFullPath($releaseDirectory).TrimEnd('\') + '\'
        if (-not $oldPath.StartsWith($releaseRoot, [StringComparison]::OrdinalIgnoreCase)) {
            throw '旧安装包路径越界，停止清理。'
        }
        Remove-Item -LiteralPath $oldPath -Force
        if (Test-Path -LiteralPath "$oldPath.sha256") {
            Remove-Item -LiteralPath "$oldPath.sha256" -Force
        }
        if (Test-Path -LiteralPath "$oldPath.sig") { Remove-Item -LiteralPath "$oldPath.sig" -Force }
    }
$manifest = [ordered]@{
    version = $config.version
    notes = '设置与关注币种管理、统一 MCP 操作确认、签名更新，以及运行中覆盖安装修复。'
    pub_date = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
    platforms = @{ 'windows-x86_64' = @{
        signature = $signature
        url = "https://github.com/panda-student/crypto-news/releases/download/v$($config.version)/$outputName"
    } }
}
$utf8NoBom = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText((Join-Path $releaseDirectory 'latest.json'), ($manifest | ConvertTo-Json -Depth 5), $utf8NoBom)
Write-Output "安装包：$destination"
Write-Output "SHA-256：$outputHash"
