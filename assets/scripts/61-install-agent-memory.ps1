# 61-install-agent-memory.ps1 — 离线安装 agent-memory CLI（只消费预编译分发物）
# 安装：<user-home>/.agent-memory/bin/agent-memory.exe
#       <user-home>/.agent-memory/scripts/recall_embedding.py（语义辅助脚本，二进制从祖先目录查找）
#       + install.json（程序目录；记录二进制与辅助脚本哈希）
# 初始化：<user-home>/.session-evidence/（仅目录骨架与初始 state，不含任何会话数据）
# 边界：不现场构建（不依赖 Rust/Python/Node）、不自动采集、不绑定知识库、不写入 PATH、不触碰既有运行时数据。
# 出错排查：assets\references\agent-memory-archive.md
param(
    [string]$UserRoot = $env:USERPROFILE,
    [string]$SuiteRoot,
    [switch]$CheckOnly,
    [switch]$Uninstall
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not $SuiteRoot) { $SuiteRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path }
if (-not (Test-Path -LiteralPath (Join-Path $SuiteRoot 'INSTALL.md') -PathType Leaf)) {
    Write-Error "不是套件根目录：$SuiteRoot（未找到 INSTALL.md）"
    exit 1
}
if (-not [System.IO.Path]::IsPathRooted($UserRoot)) {
    Write-Error "UserRoot 必须是绝对路径：$UserRoot"
    exit 1
}
. (Join-Path $SuiteRoot 'assets\scripts\agent-memory-common.ps1')

$distDir = Get-AgentMemoryDistDir -SuiteRoot $SuiteRoot
$programDir = Join-Path $UserRoot '.agent-memory'
$binDir = Join-Path $programDir 'bin'
$installedExe = Join-Path $binDir 'agent-memory.exe'
$helperDir = Join-Path $programDir 'scripts'
$installedHelper = Get-AgentMemoryHelperScriptInstallPath -UserRoot $UserRoot
$installRecord = Join-Path $programDir 'install.json'
$runtimeRoot = Join-Path $UserRoot '.session-evidence'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

# --- 0. 卸载边界：只移除程序目录，运行时根一律保留 ---
if ($Uninstall) {
    if (Test-Path -LiteralPath $programDir -PathType Container) {
        Remove-Item -LiteralPath $programDir -Recurse -Force
        Write-Host "已移除程序目录：$programDir"
    } else {
        Write-Host "程序目录不存在，无需卸载：$programDir"
    }
    if (Test-Path -LiteralPath $runtimeRoot -PathType Container) {
        Write-Host "运行时根保留：$runtimeRoot（卸载不删除任何已归档证据）"
    } else {
        Write-Host "运行时根不存在：$runtimeRoot（未创建，符合卸载预期）"
    }
    exit 0
}

# --- 1. 分发物内容校验：清单 + 二进制 + 随行许可 + 无 GUI 依赖（缺失/篡改一律显式失败） ---
try {
    $dist = Assert-AgentMemoryDistIntact -SuiteRoot $SuiteRoot -DistDir $distDir
} catch {
    Write-Error "预编译分发物不可用，离线安装终止：$($_.Exception.Message)"
    exit 1
}
$manifest = $dist.Manifest
$manifestSha256 = Get-AgentMemoryFileSha256 -Path (Join-Path $distDir 'build-manifest.json')
Write-Host "分发物：agent-memory $($manifest.version) / $($manifest.target) / SHA256 $($dist.Sha256)"

if ($CheckOnly) {
    $checks = @()
    $checks += [pscustomobject]@{ Check = '预编译分发物完整'; Ok = $true; Detail = $dist.Sha256 }
    $installedOk = Test-Path -LiteralPath $installedExe -PathType Leaf
    $installedHash = if ($installedOk) { Get-AgentMemoryFileSha256 -Path $installedExe } else { '' }
    $checks += [pscustomobject]@{ Check = '已安装二进制'; Ok = ($installedOk -and $installedHash -eq $manifest.binarySha256); Detail = $installedHash }
    $helperOk = Test-Path -LiteralPath $installedHelper -PathType Leaf
    $helperHash = if ($helperOk) { Get-AgentMemoryFileSha256 -Path $installedHelper } else { '' }
    $helperExpected = ''
    $helperDetail = ''
    try {
        $helperExpectation = Get-AgentMemoryHelperScriptExpectation -SuiteRoot $SuiteRoot
        $helperExpected = $helperExpectation.Sha256
        $helperDetail = "$helperHash（期望 $helperExpected）"
    } catch {
        $helperDetail = $_.Exception.Message
    }
    $checks += [pscustomobject]@{ Check = '语义辅助脚本'; Ok = ($helperOk -and $helperExpected -and $helperHash -eq $helperExpected); Detail = $helperDetail }
    $runtimeOk = Test-Path -LiteralPath (Join-Path $runtimeRoot 'state\schema.json') -PathType Leaf
    $checks += [pscustomobject]@{ Check = '运行时根已初始化'; Ok = $runtimeOk; Detail = $runtimeRoot }
    $checks | ConvertTo-Json -Depth 4 | Write-Host
    if (@($checks | Where-Object { -not $_.Ok }).Count -gt 0) { exit 1 }
    exit 0
}

# --- 2. 安装程序（幂等：哈希一致即跳过；运行时根不参与本步骤） ---
New-Item -ItemType Directory -Path $binDir -Force | Out-Null
$needsCopy = $true
if (Test-Path -LiteralPath $installedExe -PathType Leaf) {
    if ((Get-AgentMemoryFileSha256 -Path $installedExe) -eq $manifest.binarySha256) {
        $needsCopy = $false
        Write-Host "已安装同版本二进制，跳过复制"
    }
}
if ($needsCopy) {
    $staged = Join-Path $binDir ("agent-memory.exe.new-" + [guid]::NewGuid().ToString('N'))
    try {
        Copy-Item -LiteralPath $dist.ExePath -Destination $staged -Force
        if ((Get-AgentMemoryFileSha256 -Path $staged) -ne $manifest.binarySha256) { throw '暂存副本哈希与分发物不一致' }
        try {
            Move-Item -LiteralPath $staged -Destination $installedExe -Force
        } catch {
            # The file on disk is still the old build here, so list every process running it.
            $holders = @(Get-AgentMemoryStaleProcesses -ExePath $installedExe -AnyStartTime | ForEach-Object { if ($_.ParentName) { $_.ParentName } else { 'unknown' } }) | Sort-Object -Unique
            $holderText = if ($holders.Count -gt 0) { "（占用的宿主：$($holders -join ', ')）" } else { '' }
            throw "无法替换已安装二进制（可能仍在运行）$holderText：$($_.Exception.Message)。退出这些宿主或结束 agent-memory 进程后重跑"
        }
    } finally {
        if (Test-Path -LiteralPath $staged) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
    }
    Write-Host "已安装：$installedExe"
}

# --- 2.5 语义辅助脚本（配置 embedding 且 backend=python 时由二进制从 exe 祖先目录查找） ---
# 目标位置 <user-home>/.agent-memory/scripts/recall_embedding.py 命中 resolve_script 的首选候选；
# 缺它则 Archive 语义检索恒为 script_unavailable（词面检索照常）。幂等：同哈希即跳过。
$helperExpectation = Get-AgentMemoryHelperScriptExpectation -SuiteRoot $SuiteRoot
New-Item -ItemType Directory -Path $helperDir -Force | Out-Null
$helperNeedsCopy = $true
if (Test-Path -LiteralPath $installedHelper -PathType Leaf) {
    if ((Get-AgentMemoryFileSha256 -Path $installedHelper) -eq $helperExpectation.Sha256) {
        $helperNeedsCopy = $false
        Write-Host "已安装同哈希语义辅助脚本，跳过复制"
    }
}
if ($helperNeedsCopy) {
    $stagedHelper = Join-Path $helperDir ($script:AgentMemoryHelperScriptName + ".new-" + [guid]::NewGuid().ToString('N'))
    try {
        Copy-Item -LiteralPath $helperExpectation.Path -Destination $stagedHelper -Force
        if ((Get-AgentMemoryFileSha256 -Path $stagedHelper) -ne $helperExpectation.Sha256) { throw '暂存副本哈希与套件源不一致' }
        Move-Item -LiteralPath $stagedHelper -Destination $installedHelper -Force
    } finally {
        if (Test-Path -LiteralPath $stagedHelper) { Remove-Item -LiteralPath $stagedHelper -Force -ErrorAction SilentlyContinue }
    }
    Write-Host "已安装语义辅助脚本：$installedHelper（SHA256 $($helperExpectation.Sha256.Substring(0, 12))…）"
}

# --- 3. 自检：安装位置上的二进制可运行，版本与清单一致 ---
$versionProbe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('--version')
if ($versionProbe.ExitCode -ne 0) {
    Write-Error "安装后 --version 失败：$($versionProbe.Stderr)"
    exit 1
}
$installedVersion = $versionProbe.Stdout.Trim()
if ($installedVersion -ne "agent-memory $($manifest.version)") {
    Write-Error "安装后版本与清单不符：$installedVersion / 清单 $($manifest.version)"
    exit 1
}
$helpProbe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('--help')
if ($helpProbe.ExitCode -ne 0 -or $helpProbe.Stdout -notmatch 'agent-memory <init') {
    Write-Error "安装后 --help 失败：$($helpProbe.Stderr)"
    exit 1
}
Write-Host "自检通过：$installedVersion（--help 可用）"

# --- 4. 安装元数据（仅程序目录；内容未变则保持原文件，保证重复安装幂等） ---
$record = [ordered]@{
    component = 'agent-memory'
    version = $manifest.version
    binarySha256 = $manifest.binarySha256
    manifestSha256 = $manifestSha256
    helperScript = [ordered]@{
        path = '<user-home>/.agent-memory/scripts/recall_embedding.py'
        sha256 = $helperExpectation.Sha256
        source = 'assets/scripts/recall_embedding.py'
    }
    source = 'assets/dist/agent-memory/build-manifest.json'
    target = $manifest.target
    runtimeRoot = '<user-home>/.session-evidence'
    requiresRust = $false
    requiresPython = $false
    requiresNode = $false
}
$recordText = (($record | ConvertTo-Json -Depth 4) -replace "`r?`n", "`r`n")
$recordUnchanged = $false
if (Test-Path -LiteralPath $installRecord -PathType Leaf) {
    try {
        $existing = [System.IO.File]::ReadAllText($installRecord, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
        $recordUnchanged = ($existing.version -eq $record.version -and
            $existing.binarySha256 -eq $record.binarySha256 -and
            $existing.manifestSha256 -eq $record.manifestSha256 -and
            $existing.helperScript.sha256 -eq $record.helperScript.sha256)
    } catch { $recordUnchanged = $false }
}
if ($recordUnchanged) {
    Write-Host "安装元数据无需更新：$installRecord"
} else {
    [System.IO.File]::WriteAllText($installRecord, $recordText + "`r`n", $utf8NoBom)
    Write-Host "安装元数据已写入：$installRecord"
}

# --- 5. 空运行时根（仅目录骨架 + 初始 state；既有根原样复用，不清空、不采集） ---
$runtimeExisted = Test-Path -LiteralPath $runtimeRoot -PathType Container
$initProbe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('init', '--root', $runtimeRoot)
if ($initProbe.ExitCode -ne 0) {
    Write-Error "运行时根初始化失败：$($initProbe.Stderr)"
    exit 1
}
$initReport = $initProbe.Stdout | ConvertFrom-Json
if ($initReport.ok -ne $true) {
    Write-Error "运行时根初始化未返回 ok:true：$($initProbe.Stdout)"
    exit 1
}
$emptiness = Test-AgentMemoryRuntimeRootEmpty -RuntimeRoot $runtimeRoot
if ($runtimeExisted) {
    Write-Host "运行时根已存在，原样复用：$runtimeRoot（设备 $($initReport.deviceId)，已归档记录 $($emptiness.Records) 条）"
} else {
    Write-Host "运行时根已创建：$runtimeRoot（设备 $($initReport.deviceId)，空骨架）"
}

# --- 5.5 学习底座（幂等：已有 learning.db 则跳过；无则创建 schema v3 空库） ---
$learningRoot = Join-Path $UserRoot '.agent-memory\learning'
if (Test-Path -LiteralPath (Join-Path $learningRoot 'learning.db') -PathType Leaf) {
    Write-Host "学习底座已存在，跳过初始化：$learningRoot"
} else {
    $learnProbe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('learning', 'init', '--root', $learningRoot)
    if ($learnProbe.ExitCode -ne 0) {
        Write-Error "学习底座初始化失败：$($learnProbe.Stderr)"
        exit 1
    }
    Write-Host "学习底座已创建：$learningRoot（候选审稿管道就绪；非长期记忆）"
}

# --- 6. 边界声明（安装期不做的事） ---
Write-Host "未执行：知识库绑定、会话采集、config.local.json 生成、PATH 修改。真实采集须另行授权，见 assets\references\agent-memory-archive.md"
Write-Host "CLI 入口：$installedExe"
Write-Host "===== agent-memory $($manifest.version) 安装完成 ====="
exit 0
