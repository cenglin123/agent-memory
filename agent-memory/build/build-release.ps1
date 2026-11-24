# build-release.ps1 — agent-memory 预编译分发物构建（维护机执行，唯一产物入口）
# 产物：assets\dist\agent-memory\{agent-memory.exe, build-manifest.json, NOTICE, LICENSE}
# 谱系：工具链与 target 钉定在 agent-memory\rust-toolchain.toml（本脚本只读取，不另写版本字面量），
#       --locked release 构建 + 静态 CRT（+crt-static，目标机无需 VC++ 运行库），记录源码树哈希与二进制 SHA256。
# 目标机不跑本脚本（要求 Rust 工具链）；目标机只消费产物，见 assets\references\agent-memory-archive.md。
param(
    [string]$SuiteRoot,
    [string]$DistDir,
    [switch]$Verify,
    [switch]$RefreshLineage,
    [switch]$SkipSmoke,
    [switch]$SkipReproCheck
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not $SuiteRoot) { $SuiteRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path }
$crateRoot = Join-Path $SuiteRoot 'agent-memory'
if (-not $DistDir) { $DistDir = Join-Path $SuiteRoot 'assets\dist\agent-memory' }
. (Join-Path $SuiteRoot 'assets\scripts\agent-memory-common.ps1')

if (-not (Test-Path -LiteralPath (Join-Path $crateRoot 'Cargo.toml') -PathType Leaf)) { throw "不是套件根：$SuiteRoot（缺 agent-memory\Cargo.toml）" }

$target = 'x86_64-pc-windows-msvc'
# /Brepro：链接器用内容哈希代替时间戳与随机 PDB GUID，使不同目录、不同时间的构建字节一致。
$rustflags = '-C target-feature=+crt-static -C link-arg=/Brepro'
$manifestPath = Join-Path $DistDir 'build-manifest.json'

# --- 1. 来源谱系：只统计"构建输入"的未提交改动（tree 哈希是身份，commit 只在确实包含构建输入时记录） ---
$commit = (& cmd /c "git -C `"$SuiteRoot`" rev-parse HEAD 2>&1" | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $commit -notmatch '^[0-9a-f]{40}$') { throw "无法取得 source commit：$commit" }
$dirtyScopes = @(
    'agent-memory/Cargo.toml', 'agent-memory/Cargo.lock', 'agent-memory/rust-toolchain.toml',
    'agent-memory/build', 'agent-memory/src'
)
$dirtyArgs = ($dirtyScopes | ForEach-Object { '"' + $_ + '"' }) -join ' '
$dirtyLines = @((& cmd /c "git -C `"$SuiteRoot`" status --porcelain -- $dirtyArgs 2>&1") | Where-Object { $_ -and $_.Trim() })
$dirtyFiles = $dirtyLines.Count
$sourceFiles = Get-AgentMemorySourceFiles -SuiteRoot $SuiteRoot
$sourceTreeSha256 = Get-AgentMemorySourceTreeHash -SuiteRoot $SuiteRoot
$dirtyLabel = if ($dirtyFiles -gt 0) { "$dirtyFiles 个构建输入改动（commit 记为 null）" } else { '干净' }
Write-Host "来源：HEAD $commit；构建输入 $($sourceFiles.Count) 个文件，树哈希 $sourceTreeSha256；工作区$dirtyLabel"

function Format-Manifest {
    param([hashtable]$Values)
    $commitJson = if ($Values.TreeDirty) { 'null' } else { '"' + $Values.BaseCommit + '"' }
    $dirtyJson = if ($Values.TreeDirty) { 'true' } else { 'false' }
    $reproJson = if ($Values.Reproducible) { 'true' } else { 'null' }
    # 手写 JSON 文本而非 ConvertTo-Json：PS 5.1 对齐缩进会把空数组摊成两行，清单是长期入库的谱系记录，格式要稳定可读。
    $text = @"
{
  "schema": "agent-memory/dist-manifest@1",
  "component": "agent-memory",
  "version": "$($Values.Version)",
  "target": "$($Values.Target)",
  "profile": "release",
  "binary": "agent-memory.exe",
  "binarySha256": "$($Values.BinarySha256)",
  "binaryBytes": $($Values.BinaryBytes),
  "peHeader": "MZ",
  "toolchain": {
    "pinnedChannel": "$($Values.PinnedChannel)",
    "rustcVersion": "$($Values.RustcVersion)",
    "cargoVersion": "$($Values.CargoVersion)",
    "lockfile": "Cargo.lock",
    "locked": true,
    "crtStatic": true
  },
  "build": {
    "script": "agent-memory/build/build-release.ps1",
    "builtAtUtc": "$($Values.BuiltAtUtc)",
    "rustflags": "$($Values.Rustflags)",
    "reproducibleDoubleBuild": $reproJson
  },
  "source": {
    "commit": $commitJson,
    "baseCommit": "$($Values.BaseCommit)",
    "treeDirty": $dirtyJson,
    "treeDirtyFiles": $($Values.TreeDirtyFiles),
    "treeSha256": "$($Values.TreeSha256)",
    "treeFiles": $($Values.TreeFiles),
    "scope": [
      "agent-memory/Cargo.toml",
      "agent-memory/Cargo.lock",
      "agent-memory/rust-toolchain.toml",
      "agent-memory/build/build-release.ps1",
      "agent-memory/src/**"
    ]
  },
  "runtime": {
    "programRoot": "<user-home>/.agent-memory/bin",
    "runtimeRoot": "<user-home>/.session-evidence",
    "requiresRust": false,
    "requiresPython": false,
    "requiresNode": false,
    "guiDependencies": []
  },
  "license": {
    "license": "LICENSE",
    "notice": "NOTICE",
    "upstream": "cc-switch v4.0.20 session_manager (MIT); extraction source commit 85ad4969b3399500baaea9d46b1e08f87aba80e1"
  }
}
"@
    return $text
}

function Write-ManifestFile {
    param([hashtable]$Values)
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($manifestPath, ((Format-Manifest -Values $Values) -replace "`r?`n", "`r`n"), $utf8NoBom)
    $written = Read-AgentMemoryDistManifest -DistDir $DistDir
    if ($written.binarySha256 -ne $Values.BinarySha256 -or $written.source.treeSha256 -ne $Values.TreeSha256 -or $written.version -ne $Values.Version) {
        throw '写出的分发物清单与本次构建事实不一致（模板漂移？）'
    }
    return $written
}

# --- 2. 提交后刷新谱系：不重编译，只按当前 git 状态重写 commit/treeDirty；树与二进制哈希必须不变 ---
if ($RefreshLineage) {
    $manifest = Read-AgentMemoryDistManifest -DistDir $DistDir
    $distExe = Join-Path $DistDir $manifest.binary
    if (-not (Test-Path -LiteralPath $distExe -PathType Leaf)) { throw "预编译二进制缺失：$distExe" }
    $distHash = Get-AgentMemoryFileSha256 -Path $distExe
    if ($distHash -ne $manifest.binarySha256) { throw "分发物哈希与清单不符：$distHash / $($manifest.binarySha256)" }
    if ($sourceTreeSha256 -ne $manifest.source.treeSha256) {
        throw "构建输入已变化：当前树 $sourceTreeSha256，清单 $($manifest.source.treeSha256)。谱系刷新不能替代重建，请先无参数运行本脚本"
    }
    $values = @{
        Version = $manifest.version; Target = $manifest.target; BinarySha256 = $distHash
        BinaryBytes = (Get-Item -LiteralPath $distExe).Length
        PinnedChannel = $manifest.toolchain.pinnedChannel; RustcVersion = $manifest.toolchain.rustcVersion
        CargoVersion = $manifest.toolchain.cargoVersion; BuiltAtUtc = $manifest.build.builtAtUtc
        Rustflags = $manifest.build.rustflags; Reproducible = [bool]$manifest.build.reproducibleDoubleBuild
        BaseCommit = $commit; TreeDirty = ($dirtyFiles -gt 0); TreeDirtyFiles = $dirtyFiles
        TreeSha256 = $sourceTreeSha256; TreeFiles = $sourceFiles.Count
    }
    Write-ManifestFile -Values $values | Out-Null
    $lineage = if ($dirtyFiles -gt 0) { "commit null（仍有 $dirtyFiles 个构建输入未提交）" } else { "commit $commit（treeDirty=false）" }
    Write-Host "谱系已刷新：$lineage；树哈希 $sourceTreeSha256 与二进制 SHA256 $distHash 未变"
    exit 0
}

# --- 3. 工具链钉定：channel 与 target 都来自 rust-toolchain.toml（版本引用单点） ---
$pinned = Get-AgentMemoryPinnedToolchain -SuiteRoot $SuiteRoot
$pinnedChannel = $pinned.Channel
if ($pinned.Targets -notcontains $target) { throw "rust-toolchain.toml 未钉定 $target（实际：$($pinned.Targets -join ', ')）" }

$rustcVersion = (& cmd /c "rustc -V 2>&1" | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { throw "rustc 不可用：$rustcVersion" }
if ($rustcVersion -notmatch ('^rustc ' + [regex]::Escape($pinnedChannel) + ' \(')) {
    throw "rustc 版本与钉定不符：实际 $rustcVersion，钉定 $pinnedChannel（在 agent-memory\ 目录下 rustup 会按 rust-toolchain.toml 选版本；先 rustup toolchain install $pinnedChannel --profile minimal --target $target）"
}
$cargoVersion = (& cmd /c "cargo -V 2>&1" | Out-String).Trim()
if ($LASTEXITCODE -ne 0) { throw "cargo 不可用：$cargoVersion" }
$installedTargets = @((& cmd /c "rustup target list --installed 2>&1") -split "`n" | ForEach-Object { $_.Trim() })
if ($installedTargets -notcontains $target) { throw "target 未安装：$target（rustup target add $target --toolchain $pinnedChannel）" }
Write-Host "工具链：$rustcVersion（钉定 $pinnedChannel / $target；RUSTFLAGS=$rustflags）"

function Invoke-CargoBuild([string]$TargetDir) {
    # 静态 CRT 只作用于本次构建：临时设置 RUSTFLAGS，构建后还原调用方环境
    $savedRustflags = $env:RUSTFLAGS
    $env:RUSTFLAGS = if ($savedRustflags) { "$savedRustflags $rustflags" } else { $rustflags }
    try {
        Push-Location $crateRoot
        try {
            $targetDirArg = if ($TargetDir) { " --target-dir `"$TargetDir`"" } else { '' }
            $out = (& cmd /c "cargo build --release --locked --target $target$targetDirArg 2>&1" | Out-String)
            $code = $LASTEXITCODE
        } finally { Pop-Location }
    } finally {
        $env:RUSTFLAGS = $savedRustflags
    }
    if ($code -ne 0) { throw "cargo build 失败（exit $code）：`n$out" }
    return $out
}

# --- 4. 构建 + 可复现双构建校验（全新 target 目录重建，字节级比对） ---
# 首次构建显式传 --target-dir：cargo 遵从 CARGO_TARGET_DIR，产物路径必须与之一致，否则会读到 crate 内残留的旧 exe。
$primaryTargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $crateRoot 'target' }
$exe = Join-Path $primaryTargetDir "$target\release\agent-memory.exe"
Invoke-CargoBuild -TargetDir $primaryTargetDir | Out-Null
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "构建未产出 $exe" }
$hardened = Assert-AgentMemoryBinaryHardened -Path $exe
$hashFirst = Get-AgentMemoryFileSha256 -Path $exe
$reproducible = $null
if (-not $SkipReproCheck) {
    # 第二次构建用全新 target 目录：同目录重建会被 cargo 指纹缓存直接复用，不重新编译链接。
    $reproDir = Join-Path ([System.IO.Path]::GetTempPath()) ("agent-memory-repro-" + [guid]::NewGuid().ToString('N'))
    try {
        Invoke-CargoBuild -TargetDir $reproDir | Out-Null
        $hashSecond = Get-AgentMemoryFileSha256 -Path (Join-Path $reproDir "$target\release\agent-memory.exe")
    } finally {
        if (Test-Path -LiteralPath $reproDir) { Remove-Item -LiteralPath $reproDir -Recurse -Force -ErrorAction SilentlyContinue }
    }
    if ($hashFirst -ne $hashSecond) {
        throw "构建不可复现：同一工具链两次构建 SHA256 不同（$hashFirst / $hashSecond）；拒绝发布，见 assets\references\agent-memory-archive.md"
    }
    $reproducible = $true
    Write-Host "可复现双构建：SHA256 一致 $hashFirst"
}
Write-Host "导入表：$($hardened.Imports -join ', ')"

# --- 5. 运行时事实：版本取自构建产物本身（版本单点 = Cargo.toml） ---
$versionProbe = Invoke-AgentMemoryCli -ExePath $exe -Arguments @('--version')
if ($versionProbe.ExitCode -ne 0) { throw "构建产物 --version 失败（exit $($versionProbe.ExitCode)）：$($versionProbe.Stderr)" }
$versionText = $versionProbe.Stdout.Trim()
if ($versionText -notmatch '^agent-memory (\d+\.\d+\.\d+)$') { throw "构建产物 --version 输出非预期：$versionText" }
$version = $Matches[1]

if ($Verify) {
    $manifest = Read-AgentMemoryDistManifest -DistDir $DistDir
    if ($manifest.binarySha256 -ne $hashFirst) { throw "重建结果与分发物不一致：重建 $hashFirst，清单 $($manifest.binarySha256)" }
    if ($manifest.source.treeSha256 -ne $sourceTreeSha256) { throw "构建输入已变化：当前树 $sourceTreeSha256，清单 $($manifest.source.treeSha256)（需重新发布分发物）" }
    if ($manifest.toolchain.crtStatic -ne $true) { throw '清单未声明静态 CRT' }
    $lineageText = Assert-AgentMemoryLineage -Manifest $manifest -SuiteRoot $SuiteRoot
    Write-Host "VERIFY OK：重建产物与 assets\dist\agent-memory\build-manifest.json 一致（$hashFirst，静态 CRT，$lineageText）"
    exit 0
}

# --- 6. 发布分发物（幂等覆盖；exe 为二进制分发物，按套件惯例不入 Git） ---
New-Item -ItemType Directory -Path $DistDir -Force | Out-Null
foreach ($name in @('NOTICE', 'LICENSE')) {
    Copy-Item -LiteralPath (Join-Path $crateRoot $name) -Destination (Join-Path $DistDir $name) -Force
}
$distExe = Join-Path $DistDir 'agent-memory.exe'
Copy-Item -LiteralPath $exe -Destination $distExe -Force
$distHash = Get-AgentMemoryFileSha256 -Path $distExe
if ($distHash -ne $hashFirst) { throw "分发物复制后哈希变化：$distHash / $hashFirst" }
$distBytes = (Get-Item -LiteralPath $distExe).Length

$values = @{
    Version = $version; Target = $target; BinarySha256 = $distHash; BinaryBytes = $distBytes
    PinnedChannel = $pinnedChannel; RustcVersion = $rustcVersion; CargoVersion = $cargoVersion
    BuiltAtUtc = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    Rustflags = $rustflags; Reproducible = [bool]$reproducible
    BaseCommit = $commit; TreeDirty = ($dirtyFiles -gt 0); TreeDirtyFiles = $dirtyFiles
    TreeSha256 = $sourceTreeSha256; TreeFiles = $sourceFiles.Count
}
Write-ManifestFile -Values $values | Out-Null
Write-Host "已发布：$distExe（$distBytes 字节，SHA256 $distHash）"

# --- 7. 冒烟：分发物自身的内容校验 + CLI + 空运行时根（临时目录，不碰真实运行时根） ---
$intact = Assert-AgentMemoryDistIntact -SuiteRoot $SuiteRoot -DistDir $DistDir
if ($intact.Sha256 -ne $distHash) { throw '分发物内容校验结果与发布哈希不一致' }
if (-not $SkipSmoke) {
    $smokeRoot = Join-Path $env:TEMP ("agent-memory-build-smoke-" + [guid]::NewGuid().ToString('N'))
    try {
        $init = Invoke-AgentMemoryCli -ExePath $distExe -Arguments @('init', '--root', $smokeRoot)
        if ($init.ExitCode -ne 0) { throw "init 冒烟失败：$($init.Stderr)" }
        $help = Invoke-AgentMemoryCli -ExePath $distExe -Arguments @('--help')
        if ($help.ExitCode -ne 0 -or $help.Stdout -notmatch 'agent-memory <init') { throw "help 冒烟失败：$($help.Stderr)" }
        $status = Invoke-AgentMemoryCli -ExePath $distExe -Arguments @('status', '--root', $smokeRoot)
        if ($status.ExitCode -ne 0) { throw "status 冒烟失败：$($status.Stderr)" }
        if (($status.Stdout | ConvertFrom-Json).ok -ne $true) { throw "status 冒烟未返回 ok:true：$($status.Stdout)" }
        $empty = Test-AgentMemoryRuntimeRootEmpty -RuntimeRoot $smokeRoot
        if (-not $empty.Ok) { throw "init 建立的运行时根不干净：$($empty.Problems -join '；')" }
        Write-Host "冒烟：help / version / init / status 通过，运行时根为空白骨架"
    } finally {
        Remove-Item -LiteralPath $smokeRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "===== agent-memory $version 分发物构建完成 ====="
Write-Host "清单：$manifestPath"
Write-Host "重建校验：powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -Verify"
Write-Host "提交后刷新谱系：powershell -ExecutionPolicy Bypass -File agent-memory\build\build-release.ps1 -RefreshLineage"
