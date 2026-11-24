# 63-verify-agent-memory-lineage.ps1 — 谱系语义验收（维护机；需要 git，可选 Rust 用于 -Verify 端到端）
# 在 %TEMP% 里造一个受控 git 仓库，逐场景验证共享实现 Assert-AgentMemoryLineage：
#   正向：commit=HEAD / commit=HEAD~1 且构建输入一致 → 通过
#   反向：commit 不存在 / 非祖先 / 祖先但构建输入不一致 / treeDirty 与实际不符 → 失败
#   树哈希：同内容 CRLF / LF 检出一致，内容改动仍不一致
# 同时在两种 PowerShell 宿主（5.1 与 7）下验证；不读写真实会话目录与生产运行时根。
param(
    [string]$SuiteRoot,
    [switch]$KeepTemp
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

if (-not $SuiteRoot) { $SuiteRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path }
. (Join-Path $SuiteRoot 'assets\scripts\agent-memory-common.ps1')

$results = [System.Collections.Generic.List[object]]::new()
function Check([string]$Name, [scriptblock]$Body) {
    try {
        $detail = & $Body
        $results.Add([pscustomobject]@{ Item = $Name; Result = 'PASS'; Detail = "$detail" })
    } catch {
        $results.Add([pscustomobject]@{ Item = $Name; Result = 'FAIL'; Detail = $_.Exception.Message })
    }
}

$ps5 = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$ps7 = @(
    (Join-Path $env:ProgramFiles 'PowerShell\7\pwsh.exe'),
    (Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\pwsh.exe')
) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
$gitCommand = Get-Command git.exe -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $gitCommand) { Write-Error '需要 git（10-install-base 安装）；本脚本在临时仓库上验证谱系语义'; exit 1 }
$git = $gitCommand.Source
$cargo = Get-Command cargo.exe -ErrorAction SilentlyContinue | Select-Object -First 1

$work = Join-Path $env:TEMP ("agent-memory-lineage-" + [guid]::NewGuid().ToString('N'))
$repo = Join-Path $work 'suite'
$dist = Join-Path $repo 'assets\dist\agent-memory'
$scripts = Join-Path $repo 'assets\scripts'
$installer = Join-Path $scripts '61-install-agent-memory.ps1'
$audit = Join-Path $scripts '90-audit.ps1'
$manifestPath = Join-Path $dist 'build-manifest.json'
$probePath = Join-Path $work 'lineage-probe.ps1'
$userRoot = Join-Path $work 'user'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

# 统一走 Invoke-AgentMemoryProcess：stderr 单独收集，避开 PS 5.1 原生命令 stderr 陷阱
function Invoke-Git([string[]]$Arguments) {
    $probe = Invoke-AgentMemoryProcess -ExePath $git -Arguments (@('-C', $repo) + $Arguments)
    if ($probe.ExitCode -ne 0) { throw "git $($Arguments -join ' ') 失败（exit $($probe.ExitCode)）：$($probe.Stderr.Trim())" }
    return ($probe.Stdout.Trim())
}

function New-RepoCommit([string]$Message) {
    $null = Invoke-Git @('add', '-A')
    $tree = Invoke-Git @('write-tree')
    $parent = $null
    $headProbe = Invoke-AgentMemoryProcess -ExePath $git -Arguments @('-C', $repo, 'rev-parse', 'HEAD')
    if ($headProbe.ExitCode -eq 0 -and $headProbe.Stdout.Trim() -match '^[0-9a-f]{40}$') { $parent = $headProbe.Stdout.Trim() }
    $arguments = @('commit-tree', $tree)
    if ($parent) { $arguments += @('-p', $parent) }
    $arguments += @('-m', $Message)
    $commit = Invoke-Git $arguments
    if ($commit -notmatch '^[0-9a-f]{40}$') { throw "git commit-tree 输出非预期：$commit" }
    $null = Invoke-Git @('update-ref', 'refs/heads/main', $commit)
    return $commit
}

function Get-Head { return (Invoke-Git @('rev-parse', 'HEAD')) }

function Set-SandboxManifest([string]$CommitJson, [string]$BaseCommit, [bool]$TreeDirty, [int]$DirtyFiles) {
    $tree = Get-AgentMemorySourceTreeHash -SuiteRoot $repo
    $files = (Get-AgentMemorySourceFiles -SuiteRoot $repo).Count
    $dirtyJson = if ($TreeDirty) { 'true' } else { 'false' }
    $text = [System.IO.File]::ReadAllText($manifestPath, [System.Text.Encoding]::UTF8)
    $text = $text -replace '"commit": (null|"[0-9a-f]{40}")', ('"commit": ' + $CommitJson)
    $text = $text -replace '"baseCommit": "[0-9a-f]{40}"', ('"baseCommit": "' + $BaseCommit + '"')
    $text = $text -replace '"treeDirty": (true|false)', ('"treeDirty": ' + $dirtyJson)
    $text = $text -replace '"treeDirtyFiles": \d+', ('"treeDirtyFiles": ' + $DirtyFiles)
    $text = $text -replace '"treeSha256": "[0-9a-f]{64}"', ('"treeSha256": "' + $tree + '"')
    $text = $text -replace '"treeFiles": \d+', ('"treeFiles": ' + $files)
    [System.IO.File]::WriteAllText($manifestPath, $text, $utf8NoBom)
}

function Invoke-LineageProbe([string]$ShellExe) {
    return Invoke-AgentMemoryProcess -ExePath $ShellExe -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $probePath)
}

function Invoke-AuditProbe([string]$ShellExe) {
    return Invoke-AgentMemoryProcess -ExePath $ShellExe -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $audit, '-AgentMemoryOnly', '-UserRoot', $userRoot)
}

# 每个场景在两种宿主下各跑一次（共享实现即 90 与 -Verify 的判据）
function Check-BothShells([string]$Name, [scriptblock]$Assert) {
    $shells = @(@{ Label = 'PS 5.1'; Exe = $ps5 })
    if ($ps7) { $shells += @{ Label = 'PS 7'; Exe = $ps7 } }
    foreach ($shell in $shells) {
        Check "$Name（$($shell.Label)）" {
            $probe = Invoke-LineageProbe $shell.Exe
            & $Assert $probe
        }
    }
    return $shells.Count
}

try {
    # --- 沙箱：套件子集 + 受控 git 仓库（对象由 plumbing 生成，不改动任何既有仓库/引用） ---
    New-Item -ItemType Directory -Force -Path $scripts, $dist | Out-Null
    foreach ($name in @('61-install-agent-memory.ps1', '90-audit.ps1', 'agent-memory-common.ps1', 'opencode-desktop-common.ps1', 'recall_embedding.py')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "assets\scripts\$name") -Destination $scripts -Force
    }
    Copy-Item -Path (Join-Path $SuiteRoot 'assets\dist\agent-memory\*') -Destination $dist -Force
    New-Item -ItemType Directory -Force -Path (Join-Path $repo 'agent-memory\build'), (Join-Path $repo 'agent-memory\src') | Out-Null
    foreach ($name in @('NOTICE', 'LICENSE', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "agent-memory\$name") -Destination (Join-Path $repo "agent-memory\$name") -Force
    }
    Copy-Item -Path (Join-Path $SuiteRoot 'agent-memory\build\*') -Destination (Join-Path $repo 'agent-memory\build') -Recurse -Force
    Copy-Item -Path (Join-Path $SuiteRoot 'agent-memory\src\*') -Destination (Join-Path $repo 'agent-memory\src') -Recurse -Force
    [System.IO.File]::WriteAllText((Join-Path $repo 'INSTALL.md'), "# lineage sandbox`r`n", $utf8NoBom)
    [System.IO.File]::WriteAllText((Join-Path $repo 'notes.md'), "lineage sandbox note`r`n", $utf8NoBom)

    $env:GIT_AUTHOR_NAME = 'agent-memory-lineage'
    $env:GIT_AUTHOR_EMAIL = 'lineage@example.invalid'
    $env:GIT_COMMITTER_NAME = 'agent-memory-lineage'
    $env:GIT_COMMITTER_EMAIL = 'lineage@example.invalid'
    $null = Invoke-Git @('init', '-q')
    $null = Invoke-Git @('symbolic-ref', 'HEAD', 'refs/heads/main')
    $commit1 = New-RepoCommit 'sandbox: initial build inputs'
    $head1 = Get-Head
    if ($head1 -ne $commit1) { throw '沙箱初始提交与 HEAD 不一致' }

    [System.IO.File]::WriteAllText($probePath, @"
. '$scripts\agent-memory-common.ps1'
try {
    `$m = Read-AgentMemoryDistManifest -DistDir '$dist'
    `$text = Assert-AgentMemoryLineage -Manifest `$m -SuiteRoot '$repo'
    Write-Output ('OK|' + `$text)
    exit 0
} catch {
    Write-Output ('FAIL|' + `$_.Exception.Message)
    exit 1
}
"@, $utf8NoBom)

    # 场景 0：树哈希与换行无关——同一内容 CRLF / LF 两种检出哈希一致，内容不同仍不一致
    Check '树哈希：CRLF 与 LF 检出一致，内容改动仍可区分' {
        $eolRoot = Join-Path $work 'eol'
        New-Item -ItemType Directory -Force -Path $eolRoot | Out-Null
        Copy-Item -LiteralPath (Join-Path $repo 'agent-memory') -Destination (Join-Path $eolRoot 'agent-memory') -Recurse -Force
        $before = Get-AgentMemorySourceTreeHash -SuiteRoot $eolRoot
        $latin1 = [System.Text.Encoding]::GetEncoding(28591)
        $flipped = 0
        foreach ($rel in (Get-AgentMemorySourceFiles -SuiteRoot $eolRoot)) {
            $full = Join-Path $eolRoot ($rel -replace '/', '\')
            $text = $latin1.GetString([System.IO.File]::ReadAllBytes($full))
            $lf = $text.Replace("`r`n", "`n")
            $swapped = if ($lf -ne $text) { $lf } else { $lf.Replace("`n", "`r`n") }
            if ($swapped -ne $text) { $flipped++ }
            [System.IO.File]::WriteAllBytes($full, $latin1.GetBytes($swapped))
        }
        if ($flipped -eq 0) { throw '前置条件失败：没有文件含换行，无法验证换行归一' }
        $after = Get-AgentMemorySourceTreeHash -SuiteRoot $eolRoot
        if ($after -ne $before) { throw "换行互换 $flipped 个文件后哈希变化：$before → $after" }
        [System.IO.File]::AppendAllText((Join-Path $eolRoot 'agent-memory\Cargo.toml'), "# eol probe`n")
        $changed = Get-AgentMemorySourceTreeHash -SuiteRoot $eolRoot
        if ($changed -eq $before) { throw '内容改动后哈希未变化（归一过度）' }
        # 归一只吃 CRLF 对：孤立 CR、0x80-0xFF 非 UTF-8 字节必须仍影响哈希（防有损解码或过度裁剪）
        $probeFile = Join-Path $eolRoot 'agent-memory\src\eol_probe.bin'
        $variants = @(
            [byte[]](0x61, 0x0A, 0x62),
            [byte[]](0x61, 0x0D, 0x62),
            [byte[]](0x61, 0x80, 0x62),
            [byte[]](0x61, 0xFF, 0x62),
            [byte[]](0x61, 0x0A, 0x62, 0x20)
        )
        $seen = @{}
        foreach ($bytes in $variants) {
            [System.IO.File]::WriteAllBytes($probeFile, $bytes)
            $seen[(Get-AgentMemorySourceTreeHash -SuiteRoot $eolRoot)] = $true
        }
        if ($seen.Count -ne $variants.Count) { throw "孤立 CR / 高位字节 / 行尾空白变体的哈希发生碰撞（$($seen.Count)/$($variants.Count) 个不同）" }
        "换行互换 $flipped 个文件哈希不变；内容改动、孤立 CR、高位字节、行尾空白均改变哈希"
    }

    # 场景 1：清单 commit = HEAD，工作区干净 → 通过
    Set-SandboxManifest -CommitJson ('"' + $commit1 + '"') -BaseCommit $commit1 -TreeDirty $false -DirtyFiles 0
    Check-BothShells '正向：commit=HEAD 且构建输入一致 → 通过' {
        param($probe)
        if ($probe.ExitCode -ne 0 -or $probe.Stdout -notmatch '^OK\|') { throw "预期通过：$($probe.Stdout.Trim())$($probe.Stderr.Trim())" }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 场景 2（任务要求）：新增只改非构建输入的提交，清单 commit = HEAD~1 → 通过
    [System.IO.File]::AppendAllText((Join-Path $repo 'notes.md'), "second commit`r`n")
    $commit2 = New-RepoCommit 'sandbox: unrelated change'
    $head2 = Get-Head
    Set-SandboxManifest -CommitJson ('"' + $commit1 + '"') -BaseCommit $commit1 -TreeDirty $false -DirtyFiles 0
    Check-BothShells '正向：commit=HEAD~1 且构建输入一致 → 通过' {
        param($probe)
        if ($commit1 -eq $head2) { throw '前置条件失败：HEAD 未前进' }
        if ($probe.ExitCode -ne 0 -or $probe.Stdout -notmatch '^OK\|' -or $probe.Stdout -notmatch '祖先') {
            throw "预期通过（HEAD 祖先）：$($probe.Stdout.Trim())$($probe.Stderr.Trim())"
        }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 场景 3：commit 不存在 → 失败
    Set-SandboxManifest -CommitJson ('"' + ('0' * 40) + '"') -BaseCommit $commit2 -TreeDirty $false -DirtyFiles 0
    Check-BothShells '反向：commit 不存在 → 失败' {
        param($probe)
        if ($probe.ExitCode -eq 0) { throw "预期失败却通过：$($probe.Stdout.Trim())" }
        if ($probe.Stdout -notmatch '不存在') { throw "失败信息未指明不存在：$($probe.Stdout.Trim())" }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 场景 4：commit 存在但不是 HEAD 的祖先（独立根提交）→ 失败
    $null = Invoke-Git @('add', '-A')
    $tree = Invoke-Git @('write-tree')
    $orphan = Invoke-Git @('commit-tree', $tree, '-m', 'sandbox: unrelated root')
    if ($orphan -notmatch '^[0-9a-f]{40}$') { throw "无法创建非祖先提交对象：$orphan" }
    Set-SandboxManifest -CommitJson ('"' + $orphan + '"') -BaseCommit $commit2 -TreeDirty $false -DirtyFiles 0
    Check-BothShells '反向：commit 非 HEAD 祖先 → 失败' {
        param($probe)
        if ($probe.ExitCode -eq 0) { throw "预期失败却通过：$($probe.Stdout.Trim())" }
        if ($probe.Stdout -notmatch '祖先') { throw "失败信息未指明祖先关系：$($probe.Stdout.Trim())" }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 场景 5：祖先提交但构建输入内容已变（改了源码、清单仍指旧提交）→ 失败
    [System.IO.File]::AppendAllText((Join-Path $repo 'agent-memory\Cargo.lock'), "`r`n# lineage sandbox marker`r`n")
    $commit3 = New-RepoCommit 'sandbox: change build input'
    Set-SandboxManifest -CommitJson ('"' + $commit1 + '"') -BaseCommit $commit1 -TreeDirty $false -DirtyFiles 0
    Check-BothShells '反向：祖先提交的构建输入与工作区不一致 → 失败' {
        param($probe)
        if ($probe.ExitCode -eq 0) { throw "预期失败却通过：$($probe.Stdout.Trim())" }
        if ($probe.Stdout -notmatch '构建输入') { throw "失败信息未指明构建输入：$($probe.Stdout.Trim())" }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 场景 6：treeDirty=false 但工作区有构建输入改动 → 失败
    [System.IO.File]::AppendAllText((Join-Path $repo 'agent-memory\Cargo.lock'), "`r`n# uncommitted`r`n")
    Set-SandboxManifest -CommitJson ('"' + $commit3 + '"') -BaseCommit $commit3 -TreeDirty $false -DirtyFiles 0
    Check-BothShells '反向：treeDirty=false 但工作区有构建输入改动 → 失败' {
        param($probe)
        if ($probe.ExitCode -eq 0) { throw "预期失败却通过：$($probe.Stdout.Trim())" }
        if ($probe.Stdout -notmatch '工作区') { throw "失败信息未指明工作区改动：$($probe.Stdout.Trim())" }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 场景 7：treeDirty=true 但工作区干净（提交后未刷新谱系）→ 失败
    $null = Invoke-Git @('checkout', '--', '.')
    Set-SandboxManifest -CommitJson 'null' -BaseCommit $commit3 -TreeDirty $true -DirtyFiles 1
    Check-BothShells '反向：treeDirty=true 但工作区干净（谱系过期）→ 失败' {
        param($probe)
        if ($probe.ExitCode -eq 0) { throw "预期失败却通过：$($probe.Stdout.Trim())" }
        if ($probe.Stdout -notmatch '过期') { throw "失败信息未指明谱系过期：$($probe.Stdout.Trim())" }
        ($probe.Stdout.Trim())
    } | Out-Null

    # 端到端正向：再造一个只改非构建输入的提交（HEAD=commit4），清单 commit=commit3（HEAD~1，内容一致）
    [System.IO.File]::AppendAllText((Join-Path $repo 'notes.md'), "fourth commit`r`n")
    $commit4 = New-RepoCommit 'sandbox: docs only'
    $head4 = Get-Head
    Set-SandboxManifest -CommitJson ('"' + $commit3 + '"') -BaseCommit $commit3 -TreeDirty $false -DirtyFiles 0

    Check '端到端：HEAD~1 谱系下安装 + 90 审计通过（PS 5.1）' {
        if ($commit3 -eq $head4) { throw '前置条件失败：HEAD 未前进' }
        $install = Invoke-AgentMemoryProcess -ExePath $ps5 -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $installer, '-UserRoot', $userRoot, '-SuiteRoot', $repo)
        if ($install.ExitCode -ne 0) { throw "安装失败：$($install.Stdout.Trim())$($install.Stderr.Trim())" }
        $probe = Invoke-AuditProbe $ps5
        if ($probe.ExitCode -ne 0) { throw "审计未通过（exit $($probe.ExitCode)）：$($probe.Stdout.Trim())" }
        if ($probe.Stdout -match 'FAIL') { throw "审计出现 FAIL：$($probe.Stdout.Trim())" }
        '审计 7 项 PASS'
    }
    if ($ps7) {
        Check '端到端：HEAD~1 谱系下 90 审计通过（PS 7）' {
            $probe = Invoke-AuditProbe $ps7
            if ($probe.ExitCode -ne 0) { throw "审计未通过（exit $($probe.ExitCode)）：$($probe.Stdout.Trim())" }
            if ($probe.Stdout -match 'FAIL') { throw "审计出现 FAIL：$($probe.Stdout.Trim())" }
            '审计 7 项 PASS'
        }
    }

    # 端到端反向：非祖先 commit → 90 失败
    Set-SandboxManifest -CommitJson ('"' + $orphan + '"') -BaseCommit $commit2 -TreeDirty $false -DirtyFiles 0
    Check '端到端：非祖先 commit 下 90 审计失败（PS 5.1）' {
        $probe = Invoke-AuditProbe $ps5
        if ($probe.ExitCode -eq 0) { throw '预期审计失败却通过' }
        # 表格列宽会截断 Detail，这里只断言"出现 FAIL 且非零退出"
        if ($probe.Stdout -notmatch 'FAIL') { throw "审计未报 FAIL：$($probe.Stdout.Trim())" }
        "审计 exit $($probe.ExitCode)"
    }
    if ($ps7) {
        Check '端到端：非祖先 commit 下 90 审计失败（PS 7）' {
            $probe = Invoke-AuditProbe $ps7
            if ($probe.ExitCode -eq 0) { throw '预期审计失败却通过' }
            if ($probe.Stdout -notmatch 'FAIL') { throw "审计未报 FAIL：$($probe.Stdout.Trim())" }
            "审计 exit $($probe.ExitCode)"
        }
    }

    # 端到端：-Verify（需要钉定的 Rust 工具链；缺失时说明跳过）
    # 先在沙箱内重建一次：跨目录构建的字节不保证相同（编译器会嵌入路径），所以二进制必须与本路径一致才能校验哈希。
    Check '端到端：HEAD~1 谱系下 build -Verify 通过' {
        if (-not $cargo) { return '未安装 cargo（维护机工具），跳过；谱系断言已由共享实现直接覆盖' }
        $build = Join-Path $repo 'agent-memory\build\build-release.ps1'
        $rebuild = Invoke-AgentMemoryProcess -ExePath $ps5 -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $build, '-SuiteRoot', $repo, '-DistDir', $dist, '-SkipReproCheck')
        if ($rebuild.ExitCode -ne 0) { throw "沙箱重建失败：$($rebuild.Stdout.Trim())$($rebuild.Stderr.Trim())" }
        # 再造一个只改非构建输入的提交，使清单 commit=HEAD~1 且构建输入仍一致
        [System.IO.File]::AppendAllText((Join-Path $repo 'notes.md'), "fifth commit`r`n")
        $commit5 = New-RepoCommit 'sandbox: docs again'
        $head5 = Get-Head
        if ($commit5 -eq $commit3 -or $head5 -ne $commit5) { throw '前置条件失败：HEAD 未前进' }
        Set-SandboxManifest -CommitJson ('"' + $commit3 + '"') -BaseCommit $commit3 -TreeDirty $false -DirtyFiles 0
        $probe = Invoke-AgentMemoryProcess -ExePath $ps5 -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $build, '-Verify', '-SuiteRoot', $repo, '-DistDir', $dist)
        if ($probe.ExitCode -ne 0) { throw "VERIFY 失败：$($probe.Stdout.Trim())$($probe.Stderr.Trim())" }
        if ($probe.Stdout -notmatch 'VERIFY OK') { throw "VERIFY 未输出 OK：$($probe.Stdout.Trim())" }
        'VERIFY OK（HEAD~1 谱系）'
    }
} catch {
    $results.Add([pscustomobject]@{ Item = '谱系验收未跑完'; Result = 'FAIL'; Detail = $_.Exception.Message })
} finally {
    if (-not $KeepTemp) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
    else { Write-Host "临时仓库保留：$work" }
}

Write-Host ''
Write-Host '===== agent-memory 谱系语义验收结果 ====='
$results | Format-Table -AutoSize | Out-String | Write-Host
$failCount = @($results | Where-Object { $_.Result -eq 'FAIL' }).Count
if ($failCount -gt 0) {
    Write-Host "$failCount 项未通过；按 Detail 排查（参考 assets\references\agent-memory-archive.md）"
    exit 1
}
Write-Host "全部 $($results.Count) 项通过（临时仓库，未触碰真实会话与生产运行时根）"
exit 0
