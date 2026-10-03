# build.ps1 — svg_h.aux2 ビルド & デプロイ
#
# 使い方: .\build.ps1
#         .\build.ps1 -NoDeploy
#         .\build.ps1 -HandlerOnly   # GCMZDrops ハンドラーだけ配置する（Rust をビルドしない）

param(
    [switch]$NoDeploy,
    [switch]$HandlerOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$SrcDir = $PSScriptRoot
$AviUtl2Root = "C:\ProgramData\aviutl2"
$PluginName = "svg_h"
$PluginDir = Join-Path $AviUtl2Root "Plugin\$PluginName"

# GCMZDrops ハンドラー（.svg / .svgz → SVG_H）。正本は assets 側
$HandlerSrc = Join-Path $SrcDir "assets\GCMZScript\svg_h2obj.lua"
$GcmzScriptDir = Join-Path $AviUtl2Root "Plugin\GCMZDrops\GCMZScript"

function Deploy-GcmzHandler {
    if (-not (Test-Path -LiteralPath $HandlerSrc)) {
        throw "ハンドラーが見つかりません: $HandlerSrc"
    }
    if (-not (Test-Path -LiteralPath $GcmzScriptDir)) {
        Write-Host "  GCMZDrops が無いのでハンドラーは配置しない: $GcmzScriptDir" -ForegroundColor DarkYellow
        return
    }
    Copy-Item -LiteralPath $HandlerSrc -Destination (Join-Path $GcmzScriptDir "svg_h2obj.lua") -Force
    Write-Host "  GCMZDrops: svg_h2obj.lua（AviUtl2 の再起動後に有効）" -ForegroundColor DarkGreen
}

if ($HandlerOnly) {
    if ($NoDeploy) {
        throw "-HandlerOnly と -NoDeploy は同時に指定できません"
    }
    Write-Host "`n=== GCMZDrops ハンドラー配置 ===" -ForegroundColor Yellow
    Deploy-GcmzHandler
    Write-Host "`n完了`n" -ForegroundColor Yellow
    exit 0
}

Write-Host "`n=== $PluginName.aux2 ビルド (Rust) ===" -ForegroundColor Yellow

Push-Location $SrcDir
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed (exit $LASTEXITCODE)"
    }
} finally {
    Pop-Location
}

$ReleaseDir = Join-Path $SrcDir "target\release"
$DllName = "svg_h.dll"
$DllPath = Join-Path $ReleaseDir $DllName

if (-not (Test-Path $DllPath)) {
    Write-Error "ビルド成果物が見つかりません: $DllPath"
}

$Aux2Out = Join-Path $SrcDir "$PluginName.aux2"
Copy-Item -Path $DllPath -Destination $Aux2Out -Force
Write-Host "  完了: $Aux2Out" -ForegroundColor Green

# Language
$LangSrc = Join-Path $SrcDir "i18n\English.svg_h.aul2"
$LangDeployDir = Join-Path $AviUtl2Root "Language"
$TransSrc = Join-Path $SrcDir "translation.aul2"

if (-not $NoDeploy) {
    Write-Host "`n=== デプロイ ===" -ForegroundColor Yellow

    if (-not (Test-Path $PluginDir)) {
        New-Item -ItemType Directory -Path $PluginDir -Force | Out-Null
    }
    $DeployPath = Join-Path $PluginDir "$PluginName.aux2"
    Copy-Item -Path $Aux2Out -Destination $DeployPath -Force
    Write-Host "  配置: $DeployPath" -ForegroundColor DarkGreen

    if (Test-Path $LangSrc) {
        if (-not (Test-Path $LangDeployDir)) {
            New-Item -ItemType Directory -Path $LangDeployDir -Force | Out-Null
        }
        Copy-Item -Path $LangSrc -Destination (Join-Path $LangDeployDir "English.svg_h.aul2") -Force
        Write-Host "  Language: English.svg_h.aul2" -ForegroundColor DarkGreen
    }

    Deploy-GcmzHandler
}

Write-Host "`n全ビルド完了`n" -ForegroundColor Yellow
