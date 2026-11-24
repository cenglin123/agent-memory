# 62-verify-agent-memory-offline.ps1 — agent-memory 离线接入的独立临时环境验收（不碰真实运行时根）
# 覆盖：无 Rust/无网络条件下的安装、语义辅助脚本安装位置与哈希、CLI help、采集/覆盖查询/回读、
#       重复安装保留运行时、卸载边界，以及反向用例（缺失预编译二进制、缺失清单、篡改分发物、
#       删除/篡改已安装语义辅助脚本均须显式失败）。
# 全程只在 %TEMP% 沙箱与临时 UserRoot 内操作；不读取任何真实会话目录。
param(
    [string]$SuiteRoot,
    [switch]$KeepTemp,
    # 有计划采集在写运行时根的机器上，最后一项「全程未触碰真实运行时根」会把外部写入
    # 当成验收副作用（窗口内采集写对象）。该开关只跳过这一项，并在报告里明确标注未执行。
    [switch]$SkipRuntimeRootCheck
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

$work = Join-Path $env:TEMP ("agent-memory-offline-" + [guid]::NewGuid().ToString('N'))
$sandbox = Join-Path $work 'suite'
$sandboxScripts = Join-Path $sandbox 'assets\scripts'
$sandboxDist = Join-Path $sandbox 'assets\dist\agent-memory'
$installer = Join-Path $sandboxScripts '61-install-agent-memory.ps1'
$psExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$realRuntimeRoot = Join-Path $env:USERPROFILE '.session-evidence'
$realRuntimeExistedBefore = Test-Path -LiteralPath $realRuntimeRoot
$pwshExe = @((Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe')) | Select-Object -First 1
$pwsh7 = @(
    (Join-Path $env:ProgramFiles 'PowerShell\7\pwsh.exe'),
    (Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\pwsh.exe')
) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
$savedPath = $env:Path
$proxyKeys = @('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy')
$savedProxies = @{}
foreach ($key in $proxyKeys) { $savedProxies[$key] = [System.Environment]::GetEnvironmentVariable($key, 'Process') }

function Invoke-Installer {
    param([Parameter(Mandatory)][string]$UserRoot, [string[]]$Extra = @())
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $installer, '-UserRoot', $UserRoot, '-SuiteRoot', $sandbox) + $Extra
    return Invoke-AgentMemoryProcess -ExePath $psExe -Arguments $arguments
}

# 审计脚本在沙箱套件里运行：$PSScriptRoot 决定套件根，因此 90 只看到沙箱分发物与源码
function Invoke-AgentMemoryAudit([string]$UserRoot, [switch]$AllowArchivedData) {
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $sandboxScripts '90-audit.ps1'), '-AgentMemoryOnly', '-UserRoot', $UserRoot)
    if ($AllowArchivedData) { $arguments += '-AllowArchivedData' }
    return Invoke-AgentMemoryProcess -ExePath $psExe -Arguments $arguments
}

function New-SyntheticRollout([string]$Path) {
    # 合成 codex rollout（两行：会话元数据 + 一条助手消息）；内容为虚构，不含任何真实会话
    $lines = @(
        '{"timestamp":"2026-10-26T00:00:00Z","type":"session_meta","payload":{"id":"sess-p6-offline","cwd":"/tmp/synthetic-p6"}}',
        '{"timestamp":"2026-10-26T00:00:01Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"synthetic-p6-line"}]}}'
    )
    [System.IO.File]::WriteAllText($Path, ($lines -join "`r`n") + "`r`n", $utf8NoBom)
    return $lines
}

function Get-RecordsJson([string]$RuntimeRoot) { return (Join-Path $RuntimeRoot 'state\records.json') }

# 合成 recall.json：单活源 codex + 显式 cache_root。仅字段足够跑 index/search。
function New-RecallIndexConfig([string]$Path, [string]$SourceRoot, [string]$CacheRoot) {
    $config = [ordered]@{
        version = 2
        sources = @([ordered]@{
            source_id = 'codex'
            kind = 'codex'
            root = ($SourceRoot -replace '\\', '/')
            enabled = $true
        })
        source_order = @('codex')
        interpreter = [ordered]@{ kb = [ordered]@{ command = 'C:/python/python.exe' } }
        domain = 'default'
        telemetry = [ordered]@{ enabled = $false }
        cache_root = ($CacheRoot -replace '\\', '/')
    }
    [System.IO.File]::WriteAllText($Path, (($config | ConvertTo-Json -Depth 8) + "`r`n"), $utf8NoBom)
}

function Invoke-SyntheticSearch([string]$ConfigPath, $Request) {
    $json = $Request | ConvertTo-Json -Compress -Depth 6
    return Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('memory_search', '--config', $ConfigPath, '--request', $json)
}

function Find-SourceStatus($Response, [string]$SourceId) {
    foreach ($entry in @($Response.source_status)) {
        if ("$($entry.source_id)" -eq $SourceId) { return $entry }
    }
    return $null
}

# 真实运行时根的只读指纹：state/ 与 archive/ 下每个文件的相对路径 + 大小 + SHA256（不解析、不复制内容）。
# 用于证明验收过程从未改写用户既有的证据层；哈希是对完整性取指纹，不读取会话语义。
function Get-AgentMemoryRuntimeFingerprint([string]$Root) {
    if (-not (Test-Path -LiteralPath $Root -PathType Container)) { return $null }
    # Canonicalize: %TEMP% can be an 8.3 short path while enumerated files come
    # back long, and the relative path is derived by string subtraction.
    $Root = Resolve-AgentMemoryLongPath -Path $Root
    $entries = [System.Collections.Generic.List[string]]::new()
    foreach ($sub in @('state', 'archive')) {
        $dir = Join-Path $Root $sub
        if (-not (Test-Path -LiteralPath $dir -PathType Container)) { continue }
        foreach ($file in Get-ChildItem -LiteralPath $dir -Recurse -File -Force -ErrorAction Stop) {
            $relative = $file.FullName.Substring($Root.Length).TrimStart('\')
            $entries.Add(('{0}|{1}|{2}' -f $relative, $file.Length, (Get-AgentMemoryFileSha256 -Path $file.FullName)))
        }
    }
    $sorted = $entries.ToArray()
    [System.Array]::Sort($sorted, [System.StringComparer]::Ordinal)
    $joined = $sorted -join "`n"
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { $digest = ([System.BitConverter]::ToString($sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($joined))) -replace '-', '').ToLowerInvariant() } finally { $sha.Dispose() }
    return [pscustomobject]@{ Files = $sorted.Count; Digest = $digest; Entries = $sorted }
}

function Assert-ExitNonZero($Probe, [string]$What) {
    if ($Probe.ExitCode -eq 0) { throw "$What 未显式失败（exit 0）" }
    return "exit $($Probe.ExitCode)"
}

function Assert-FailureMentions($Probe, [string]$Pattern, [string]$What) {
    $message = $Probe.Stdout + "`n" + $Probe.Stderr
    if ($message -notmatch $Pattern) { throw "$What 的失败信息未指明原因（期望匹配 $Pattern）：$($message.Trim())" }
}

try {
    # 真实运行时根只做只读指纹（存在性与 state/archive 内容哈希），开始前拍一次快照
    $script:realBefore = Get-AgentMemoryRuntimeFingerprint -Root $realRuntimeRoot

    # --- 沙箱：只复制安装必需的脚本与分发物，模拟"目标机只有套件与预编译产物" ---
    New-Item -ItemType Directory -Force -Path $sandboxScripts, $sandboxDist | Out-Null
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\61-install-agent-memory.ps1') -Destination $sandboxScripts -Force
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\agent-memory-common.ps1') -Destination $sandboxScripts -Force
    # 语义辅助脚本：61 从套件源安装它，90 用它核对已安装副本（缺失即 FAIL，反向用例会删改它）
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\recall_embedding.py') -Destination $sandboxScripts -Force
    # 审计脚本与其依赖：审计按 $PSScriptRoot 解析套件根，这样反向用例只影响沙箱
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\90-audit.ps1') -Destination $sandboxScripts -Force
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\opencode-desktop-common.ps1') -Destination $sandboxScripts -Force
    # 90 的 AM-* 项按需点源的 agent-memory 模块（缺失时这些项只能报「无法检查」WARN，沙箱验收覆盖不到）
    foreach ($name in @('agent-memory-inject-hook.ps1', 'agent-memory-reflect.ps1', 'agent-memory-qualify.ps1', 'agent-memory-ground.ps1', 'agent-memory-skill.ps1', 'agent-memory-refresh.ps1', 'agent-memory-opencode-brief.ps1', 'agent-memory-codex-brief.ps1', 'agent-memory-dsh-brief.ps1', 'agent-memory-workbuddy-brief.ps1', 'agent-memory-hermes-brief.ps1', 'agent-memory-embedding-sidecar.ps1')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "assets\scripts\$name") -Destination $sandboxScripts -Force
    }
    # AM-DSH-BRIEF 还核对套件插件源与 preflight 探针；只部署运行文件，测试目录不进沙箱。
    $sandboxDshPlugin = Join-Path $sandbox 'assets\dsh\plugins\agent-memory-brief'
    New-Item -ItemType Directory -Force -Path $sandboxDshPlugin | Out-Null
    foreach ($name in @('index.js', 'factory.js', 'package.json')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "assets\dsh\plugins\agent-memory-brief\$name") -Destination $sandboxDshPlugin -Force
    }
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\preflight-dsh-brief.mjs') -Destination $sandboxScripts -Force
    # AM-CODEX-BRIEF 还核对 wrapper 套件源（部署目标与源哈希一致才算收养）
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\recall_brief_hook.py') -Destination $sandboxScripts -Force
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\agent_memory_embedding_sidecar.py') -Destination $sandboxScripts -Force
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\workbuddy_brief_hook.py') -Destination $sandboxScripts -Force
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\preflight-hermes-brief.py') -Destination $sandboxScripts -Force
    # AM-HERMES-BRIEF 还核对套件插件源；只部署运行文件。
    $sandboxHermesPlugin = Join-Path $sandbox 'assets\hermes\plugins\agent-memory-brief'
    New-Item -ItemType Directory -Force -Path $sandboxHermesPlugin | Out-Null
    foreach ($name in @('plugin.yaml', '__init__.py')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "assets\hermes\plugins\agent-memory-brief\$name") -Destination $sandboxHermesPlugin -Force
    }
    # AM-OPENCODE-BRIEF 还核对套件插件源；只部署运行文件，测试目录不进沙箱。
    $sandboxPlugin = Join-Path $sandbox 'assets\opencode\plugins\agent-memory-brief'
    New-Item -ItemType Directory -Force -Path $sandboxPlugin | Out-Null
    foreach ($name in @('index.js', 'package.json')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "assets\opencode\plugins\agent-memory-brief\$name") -Destination $sandboxPlugin -Force
    }
    # AM-SKILL 用 recall_onboarding.py 对照套件技能来源审计分发（沙箱技能分发为空，按设计 WARN 而非「无法检查」）
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\scripts\recall_onboarding.py') -Destination $sandboxScripts -Force
    New-Item -ItemType Directory -Force -Path (Join-Path $sandbox 'assets\skills') | Out-Null
    Copy-Item -LiteralPath (Join-Path $SuiteRoot 'assets\skills\agent-memory') -Destination (Join-Path $sandbox 'assets\skills') -Recurse -Force
    Copy-Item -Path (Join-Path $SuiteRoot 'assets\dist\agent-memory\*') -Destination $sandboxDist -Force
    # 许可与构建输入：安装脚本校验许可来源，审计校验源码树哈希
    New-Item -ItemType Directory -Force -Path (Join-Path $sandbox 'agent-memory\build') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $sandbox 'agent-memory\src') | Out-Null
    foreach ($name in @('NOTICE', 'LICENSE', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml')) {
        Copy-Item -LiteralPath (Join-Path $SuiteRoot "agent-memory\$name") -Destination (Join-Path $sandbox "agent-memory\$name") -Force
    }
    Copy-Item -Path (Join-Path $SuiteRoot 'agent-memory\build\*') -Destination (Join-Path $sandbox 'agent-memory\build') -Recurse -Force
    Copy-Item -Path (Join-Path $SuiteRoot 'agent-memory\src\*') -Destination (Join-Path $sandbox 'agent-memory\src') -Recurse -Force
    [System.IO.File]::WriteAllText((Join-Path $sandbox 'INSTALL.md'), "# offline verification sandbox`r`n", $utf8NoBom)
    $manifest = Read-AgentMemoryDistManifest -DistDir $sandboxDist

    # --- 模拟无 Rust / 无网络：PATH 只留系统目录，代理指向不可用端口 ---
    $env:Path = "$env:SystemRoot\System32;$env:SystemRoot"
    foreach ($key in $proxyKeys) { [System.Environment]::SetEnvironmentVariable($key, 'http://127.0.0.1:9', 'Process') }

    $userA = Join-Path $work 'userA'
    $runtimeA = Join-Path $userA '.session-evidence'
    $installedExe = Join-Path $userA '.agent-memory\bin\agent-memory.exe'

    Check '离线环境确实没有 Rust 工具链' {
        $found = @(Get-Command rustc, cargo -ErrorAction SilentlyContinue)
        if ($found.Count -gt 0) { throw "PATH 中仍有 Rust：$($found.Source -join ', ')" }
        'rustc/cargo 均不可解析'
    }

    Check '安装脚本自身不含网络调用' {
        $patterns = 'Invoke-WebRequest|Invoke-RestMethod|Start-BitsTransfer|Net\.WebClient|curl\.exe|wget|https?://'
        $hits = @(Select-String -LiteralPath $installer, (Join-Path $sandboxScripts 'agent-memory-common.ps1') -Pattern $patterns -ErrorAction Stop)
        if ($hits.Count -gt 0) { throw "安装路径出现网络调用：$($hits.Line.Trim() -join ' | ')" }
        '无 HTTP/下载调用'
    }

    Check '预编译分发物完整（清单 + 二进制 + 许可 + 无 GUI 依赖）' {
        $intact = Assert-AgentMemoryDistIntact -SuiteRoot $sandbox -DistDir $sandboxDist
        $pinned = Get-AgentMemoryPinnedToolchain -SuiteRoot $sandbox
        if ($intact.Manifest.toolchain.pinnedChannel -ne $pinned.Channel) { throw "分发物工具链 $($intact.Manifest.toolchain.pinnedChannel) 与钉定 $($pinned.Channel) 不一致" }
        "$($intact.Manifest.version) $($intact.Sha256)（工具链 $($pinned.Channel) / $($intact.Manifest.target)）"
    }

    Check '静态 CRT：导入表无 VCRUNTIME / api-ms-win-crt（离线无需 VC++ 运行库）' {
        $intact = Assert-AgentMemoryDistIntact -SuiteRoot $sandbox -DistDir $sandboxDist
        if ($intact.Manifest.toolchain.crtStatic -ne $true) { throw '清单未声明 crtStatic=true' }
        if ("$($intact.Manifest.build.rustflags)" -notmatch 'crt-static') { throw "清单未记录 crt-static：$($intact.Manifest.build.rustflags)" }
        $imports = @($intact.Imports)
        if ($imports.Count -eq 0) { throw '未解析到导入表' }
        $dynamic = @($imports | Where-Object { $_ -match '^(?i)(vcruntime\d+|msvcp\d+|ucrtbase|api-ms-win-crt-.*|concrt\d+)\.dll$' })
        if ($dynamic.Count -gt 0) { throw "仍依赖动态 CRT：$($dynamic -join ', ')" }
        "导入 $($imports -join ', ')"
    }

    Check 'PowerShell 7 与 5.1 读取同一清单' {
        if (-not $pwsh7) { return '未安装 PowerShell 7，跳过（PS 5.1 路径已覆盖）' }
        $commonPath = Join-Path $sandboxScripts 'agent-memory-common.ps1'
        $scriptText = ". '$commonPath'; `$m = Read-AgentMemoryDistManifest -DistDir '$sandboxDist'; if (`$m.binarySha256 -ne '$($manifest.binarySha256)') { throw 'hash mismatch' }; `$m.build.builtAtUtc"
        $probe = Invoke-AgentMemoryProcess -ExePath $pwsh7 -Arguments @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-Command', $scriptText)
        if ($probe.ExitCode -ne 0) { throw "pwsh 读取清单失败（exit $($probe.ExitCode)）：$($probe.Stderr.Trim())" }
        $builtAt = $probe.Stdout.Trim()
        if ($builtAt -notmatch '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$') { throw "pwsh 归一后的 builtAtUtc 非预期：$builtAt" }
        "pwsh $(& $pwsh7 -NoProfile -Command '$PSVersionTable.PSVersion.ToString()') 读取通过（$builtAt）"
    }

    Check '离线安装到临时 UserRoot（程序目录 + 空运行时根）' {
        $probe = Invoke-Installer -UserRoot $userA
        if ($probe.ExitCode -ne 0) { throw "install exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) { throw "未安装二进制：$installedExe" }
        $installedHash = Get-AgentMemoryFileSha256 -Path $installedExe
        if ($installedHash -ne $manifest.binarySha256) { throw "安装二进制哈希与分发物不符：$installedHash / $($manifest.binarySha256)" }
        if (-not (Test-Path -LiteralPath (Join-Path $userA '.agent-memory\install.json') -PathType Leaf)) { throw '缺安装元数据 install.json' }
        $empty = Test-AgentMemoryRuntimeRootEmpty -RuntimeRoot $runtimeA
        if (-not $empty.Ok) { throw "运行时根不是空白骨架：$($empty.Problems -join '；')" }
        if (Test-Path -LiteralPath (Join-Path $runtimeA 'state\sources.json')) { throw '安装期写入了数据源登记（不得自动采集）' }
        "已安装 $($manifest.version)，运行时根空白"
    }

    Check '语义辅助脚本装到 .agent-memory\scripts 且与套件源哈希一致' {
        # 二进制从 exe 目录及其祖先查找 scripts\recall_embedding.py（embed.rs resolve_script）；
        # exe 在 .agent-memory\bin\，因此祖先 .agent-memory\scripts\ 必须命中。
        $helperPath = Join-Path $userA '.agent-memory\scripts\recall_embedding.py'
        if (-not (Test-Path -LiteralPath $helperPath -PathType Leaf)) { throw "未安装语义辅助脚本：$helperPath（语义检索将恒为 script_unavailable）" }
        $expected = Get-AgentMemoryFileSha256 -Path (Join-Path $SuiteRoot 'assets\scripts\recall_embedding.py')
        $actual = Get-AgentMemoryFileSha256 -Path $helperPath
        if ($actual -ne $expected) { throw "辅助脚本哈希与套件源不符：$actual / $expected" }
        $recordPath = Join-Path $userA '.agent-memory\install.json'
        $record = [System.IO.File]::ReadAllText($recordPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
        if ("$($record.helperScript.sha256)" -ne $actual) { throw "install.json 未记录辅助脚本哈希：$($record.helperScript.sha256) / $actual" }
        "SHA256 $($actual.Substring(0, 12))…，install.json 已记录"
    }

    Check '安装后 CLI help / version 可运行' {
        $help = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('--help')
        if ($help.ExitCode -ne 0) { throw "help exit $($help.ExitCode)：$($help.Stderr)" }
        foreach ($token in @('capture', 'status', 'coverage', 'read', 'scrub')) {
            if ($help.Stdout -notmatch [regex]::Escape($token)) { throw "help 未列出 $token" }
        }
        $version = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('--version')
        if ($version.ExitCode -ne 0) { throw "version exit $($version.ExitCode)：$($version.Stderr)" }
        if ($version.Stdout.Trim() -ne "agent-memory $($manifest.version)") { throw "版本输出非预期：$($version.Stdout.Trim())" }
        "help 与 $($version.Stdout.Trim()) 正常"
    }

    Check '审计 -AgentMemoryOnly 在空白运行时根全 PASS' {
        $probe = Invoke-AgentMemoryAudit -UserRoot $userA
        if ($probe.ExitCode -ne 0) { throw "audit exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        if ($probe.Stdout -notmatch 'agent-memory 构建谱系与源码一致') { throw "审计未执行 agent-memory 谱系检查：$($probe.Stdout)" }
        if ($probe.Stdout -notmatch '语义辅助脚本已安装且与分发物一致') { throw "审计未执行语义辅助脚本检查：$($probe.Stdout)" }
        if ($probe.Stdout -match '无法检查') { throw "沙箱缺审计依赖，AM-* 项报「无法检查」：$($probe.Stdout)" }
        '预编译分发物 / 谱系 / 安装 / 辅助脚本 / GUI / 空运行时根 全 PASS'
    }

    $syntheticSource = Join-Path $work 'synthetic\rollout.jsonl'
    New-Item -ItemType Directory -Force -Path (Split-Path $syntheticSource) | Out-Null
    $syntheticLines = @(New-SyntheticRollout -Path $syntheticSource)
    $sourceHashBefore = Get-AgentMemoryFileSha256 -Path $syntheticSource

    Check '采集合成 fixture（只读源）' {
        $capture = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('capture', '--root', $runtimeA, '--source', $syntheticSource, '--provider', 'codex', '--domain', 'default')
        if ($capture.ExitCode -ne 0) { throw "capture exit $($capture.ExitCode)：$($capture.Stderr)" }
        $report = $capture.Stdout | ConvertFrom-Json
        if ($report.ok -ne $true -or $report.recordsNew -ne 2) { throw "采集结果非预期：$($capture.Stdout)" }
        if ((Get-AgentMemoryFileSha256 -Path $syntheticSource) -ne $sourceHashBefore) { throw '采集改写了源文件（必须只读）' }
        "$($report.recordsNew) 条记录，源文件哈希未变"
    }

    Check '覆盖查询返回机器可读 JSON' {
        $coverage = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('coverage', '--root', $runtimeA)
        if ($coverage.ExitCode -ne 0) { throw "coverage exit $($coverage.ExitCode)：$($coverage.Stderr)" }
        $report = $coverage.Stdout | ConvertFrom-Json
        if ($report.ok -ne $true) { throw "coverage 未返回 ok:true：$($coverage.Stdout)" }
        if ($report.totals.sources -ne 1 -or $report.totals.recordsCommitted -ne 2) { throw "覆盖计数非预期：sources=$($report.totals.sources) records=$($report.totals.recordsCommitted)" }
        if (-not ($report.sources[0].provider -eq 'codex')) { throw '覆盖报告 provider 非预期' }
        "sources=$($report.totals.sources) records=$($report.totals.recordsCommitted)"
    }

    Check '按记录引用回读并校验哈希' {
        $recordsPath = Get-RecordsJson -RuntimeRoot $runtimeA
        if (-not (Test-Path -LiteralPath $recordsPath -PathType Leaf)) { throw '缺 state\records.json' }
        $records = [System.IO.File]::ReadAllText($recordsPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
        $sha = @($records.records.PSObject.Properties.Name) | Select-Object -First 1
        if (-not $sha) { throw '记录索引为空' }
        $outFile = Join-Path $work 'readback.txt'
        $read = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('read', '--root', $runtimeA, '--sha256', $sha, '--domain', 'default', '--out', $outFile)
        if ($read.ExitCode -ne 0) { throw "read exit $($read.ExitCode)：$($read.Stderr)" }
        $report = $read.Stdout | ConvertFrom-Json
        if ($report.verified -ne $true) { throw '回读未通过哈希校验' }
        $body = [System.IO.File]::ReadAllText($outFile, [System.Text.Encoding]::UTF8).TrimEnd("`r", "`n")
        if ($syntheticLines -notcontains $body) { throw "回读内容与合成源记录不一致：$body" }
        "kind=$($report.kind) bytes=$($report.bytes) verified=$($report.verified) 内容与源记录一致"
    }

    # --- S2.5：对象压缩存储、compact 迁移与压缩对象凭据扫描 ---
    $script:s25ObjectDir = Join-Path $runtimeA 'archive\objects'

    Check 'S2.5：JSONL 采集对象以压缩编码存储且回读逐字节一致' {
        # 新写入的可压缩对象必须落在 <sha256>.zst；明文回读经 CLI read 校验。
        $encoded = @(Get-ChildItem -LiteralPath $script:s25ObjectDir -Recurse -File -Filter '*.zst' -Force -ErrorAction Stop)
        if ($encoded.Count -lt 1) { throw '采集后没有任何 <sha256>.zst 压缩对象' }
        foreach ($obj in $encoded) {
            if ((Split-Path $obj.DirectoryName -Leaf).Length -ne 2) { throw "压缩对象不在两段分片目录：$($obj.FullName)" }
            if ($obj.BaseName -notmatch '^[0-9a-f]{64}$') { throw "压缩对象名不是内容地址：$($obj.Name)" }
        }
        "压缩对象 $($encoded.Count) 个（显式 .zst 后缀编码）"
    }

    Check 'S2.5：compact --dry-run 不写运行时根（state+archive 逐文件指纹一致）' {
        $before = Get-AgentMemoryRuntimeFingerprint -Root $runtimeA
        $probe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('compact', '--root', $runtimeA, '--dry-run')
        if ($probe.ExitCode -ne 0) { throw "compact --dry-run exit $($probe.ExitCode)：$($probe.Stderr)" }
        $report = $probe.Stdout | ConvertFrom-Json
        if ($report.dryRun -ne $true) { throw 'compact 报告未标注 dryRun=true' }
        $after = Get-AgentMemoryRuntimeFingerprint -Root $runtimeA
        if ($after.Digest -ne $before.Digest) { throw '--dry-run 改写了运行时根（state/archive 指纹变化）' }
        "dryRun 报告 compressed=$($report.compressed)；指纹 $($after.Digest.Substring(0, 12))… 前后一致"
    }

    Check 'S2.5：compact 迁移遗留原样对象、回读一致且幂等' {
        # 人工降级：把压缩对象解回明文并落回内容地址（模拟 S2.5 之前的存量形态），再 compact。
        $legacy = @()
        foreach ($obj in @(Get-ChildItem -LiteralPath $script:s25ObjectDir -Recurse -File -Filter '*.zst' -Force)) {
            $decoded = Join-Path $work ('legacy-' + $obj.BaseName + '.bin')
            $read = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('read', '--root', $runtimeA, '--sha256', $obj.BaseName, '--domain', 'default', '--out', $decoded)
            if ($read.ExitCode -ne 0) { throw "降级前回读失败：$($read.Stderr)" }
            $target = Join-Path $obj.DirectoryName $obj.BaseName
            Copy-Item -LiteralPath $decoded -Destination $target -Force
            Remove-Item -LiteralPath $obj.FullName -Force
            $legacy += @{ Sha = $obj.BaseName; Plain = $decoded; Dir = $obj.DirectoryName }
        }
        if ($legacy.Count -lt 1) { throw '前置条件失败：没有可降级的压缩对象' }
        $probe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('compact', '--root', $runtimeA)
        if ($probe.ExitCode -ne 0) { throw "compact exit $($probe.ExitCode)：$($probe.Stderr)" }
        $report = $probe.Stdout | ConvertFrom-Json
        if ($report.compressed -lt 1) { throw "compact 未迁移原样对象：$($probe.Stdout)" }
        if ($report.savedBytes -le 0) { throw "compact 未节省字节：$($probe.Stdout)" }
        foreach ($item in $legacy) {
            $zst = Join-Path $item.Dir ($item.Sha + '.zst')
            if (-not (Test-Path -LiteralPath $zst -PathType Leaf)) { throw "迁移后缺压缩对象：$zst" }
            if (Test-Path -LiteralPath (Join-Path $item.Dir $item.Sha) -PathType Leaf) { throw "迁移后明文残留：$($item.Sha)" }
            $out = Join-Path $work ('verify-' + $item.Sha + '.bin')
            $read = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('read', '--root', $runtimeA, '--sha256', $item.Sha, '--domain', 'default', '--out', $out)
            if ($read.ExitCode -ne 0) { throw "迁移后回读失败 $($item.Sha)：$($read.Stderr)" }
            if ((Get-AgentMemoryFileSha256 -Path $out) -ne (Get-AgentMemoryFileSha256 -Path $item.Plain)) { throw "迁移后回读内容不一致：$($item.Sha)" }
        }
        $again = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('compact', '--root', $runtimeA)
        if ($again.ExitCode -ne 0) { throw "compact 幂等轮 exit $($again.ExitCode)：$($again.Stderr)" }
        $againReport = $again.Stdout | ConvertFrom-Json
        if ($againReport.compressed -ne 0) { throw "compact 不幂等：第二次 compressed=$($againReport.compressed)" }
        "迁移 $($report.compressed) 个对象、节省 $($report.savedBytes) 字节；第二轮 compressed=0（幂等），回读逐字节一致"
    }

    Check 'S2.5：status 存储统计按编码分组' {
        $probe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('status', '--root', $runtimeA)
        if ($probe.ExitCode -ne 0) { throw "status exit $($probe.ExitCode)：$($probe.Stderr)" }
        $projection = $probe.Stdout | ConvertFrom-Json
        $storage = $projection.reconcile.storage
        if (-not $storage) { throw 'status 报告缺 reconcile.storage' }
        $zstd = [int64]$storage.zstdObjects
        $plain = [int64]$storage.plainObjects
        if (($zstd + $plain) -ne [int64]$projection.reconcile.objects_on_disk) { throw "存储分组计数与对象总数不符：zstd=$zstd plain=$plain 总数=$($projection.reconcile.objects_on_disk)" }
        if ($zstd -lt 1) { throw '压缩组为空（采集对象应为 zstd 编码）' }
        if ([int64]$storage.zstdStoredBytes -ge [int64]$storage.zstdOriginalBytes) { throw '压缩组 stored 未小于 original' }
        "plain=$($plain)/$($storage.plainStoredBytes)B zstd=$($zstd)/$($storage.zstdStoredBytes)B（原始 $($storage.zstdOriginalBytes)B）"
    }

    # --- S2.5：压缩对象内的凭据提醒（合成值运行时拼接，交付物不出现可命中字面量）---
    $script:s25FakeKey = 'sk-' + ('s2' + '5' + 'sample' + 'abcdefghijklmnop')
    $leakSource = Join-Path $work 'synthetic\rollout-leak.jsonl'
    $leakLines = @(
        '{"timestamp":"2026-10-26T01:00:00Z","type":"session_meta","payload":{"id":"sess-leak","cwd":"/tmp/synthetic-leak"}}',
        ('{"timestamp":"2026-10-26T01:00:01Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"leak probe ' + $script:s25FakeKey + ' end"}]}}')
    )
    [System.IO.File]::WriteAllText($leakSource, ($leakLines -join "`r`n") + "`r`n", $utf8NoBom)

    Check 'S2.5：压缩对象内凭据 audit-scan 计数命中且不打印原值' {
        $capture = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('capture', '--root', $runtimeA, '--source', $leakSource, '--provider', 'codex', '--domain', 'default')
        if ($capture.ExitCode -ne 0) { throw "capture exit $($capture.ExitCode)：$($capture.Stderr)" }
        $rulesFile = Join-Path $work 'audit-rules.json'
        $rulesPayload = @(Get-AgentMemoryCredentialRules | ForEach-Object { [ordered]@{ name = $_.Name; pattern = $_.Pattern } })
        [System.IO.File]::WriteAllText($rulesFile, (($rulesPayload | ConvertTo-Json -Depth 4) + "`r`n"), $utf8NoBom)
        try {
            $scan = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('audit-scan', '--root', $runtimeA, '--patterns', "@$rulesFile")
            if ($scan.ExitCode -ne 0) { throw "audit-scan exit $($scan.ExitCode)：$($scan.Stderr)" }
        } finally { Remove-Item -LiteralPath $rulesFile -Force -ErrorAction SilentlyContinue }
        $projection = $scan.Stdout | ConvertFrom-Json
        if ([int]$projection.filesWithHits -ne 1) { throw "命中对象数非预期：$($scan.Stdout)" }
        if ([int]$projection.hitsByRule.'known-key-prefix' -ne 1) { throw "known-key-prefix 计数非预期：$($scan.Stdout)" }
        if ($scan.Stdout -match [regex]::Escape($script:s25FakeKey)) { throw 'audit-scan 报告打印了匹配原值' }
        "命中 $($projection.filesWithHits)/$($projection.objects) 个对象（known-key-prefix x1）；报告只含计数不含原值"
    }

    Check 'S2.5：运行时根对象含凭据时 90 审计 WARN 不 FAIL 且 Detail 无原值' {
        $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
        if ($probe.ExitCode -ne 0) { throw "对象凭据提醒不应改变审计退出码（exit $($probe.ExitCode)）：$($probe.Stdout)$($probe.Stderr)" }
        $text = $probe.Stdout + "`n" + $probe.Stderr
        if ($text -notmatch '运行时根凭据扫描') { throw '审计没有执行运行时根凭据扫描' }
        if ($text -notmatch 'WARN') { throw '审计输出缺少 WARN 行' }
        if ($text -match [regex]::Escape($script:s25FakeKey)) { throw '审计 Detail 泄露了匹配原值' }
        '对象命中记 WARN、审计退出码 0、Detail 只含计数'
    }

    # --- 全文检索（L6）：合成 Codex JSONL + 分发物二进制，临时 cache_root ---
    $l6Root = Join-Path $work 'l6-index'
    $l6Codex = Join-Path $l6Root 'codex'
    $l6Sessions = Join-Path $l6Codex 'sessions'
    New-Item -ItemType Directory -Force -Path $l6Sessions | Out-Null
    Copy-Item -LiteralPath $syntheticSource -Destination (Join-Path $l6Sessions 'rollout.jsonl') -Force
    $l6Config = Join-Path $l6Root 'recall.json'
    $l6Cache = Join-Path $l6Root 'cache'
    New-RecallIndexConfig -Path $l6Config -SourceRoot $l6Codex -CacheRoot $l6Cache
    $l6Query = 'synthetic-p6-line'

    Check '全文索引：分发物对合成 Codex JSONL 建索引' {
        $probe = Invoke-AgentMemoryCli -ExePath $installedExe -Arguments @('index', '--config', $l6Config)
        if ($probe.ExitCode -ne 0) { throw "index exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        $report = $probe.Stdout | ConvertFrom-Json
        if ($report.ok -ne $true) { throw "index 未返回 ok:true：$($probe.Stdout)" }
        $source = @($report.sources)[0]
        if ("$($source.status)" -notin @('built', 'updated', 'unchanged')) { throw "索引状态非预期：$($source.status)" }
        if ([int64]$source.rows -lt 1) { throw "索引行数为 0：$($probe.Stdout)" }
        if (-not (Test-Path -LiteralPath (Join-Path $l6Cache 'live\codex\fts.sqlite') -PathType Leaf)) {
            throw '未生成 live\codex\fts.sqlite'
        }
        "status=$($source.status) rows=$($source.rows) vectors=$(if ($source.detail.vectors) { $source.detail.vectors.status } else { 'null' })"
    }

    Check '全文索引：缺省模式解析为 hybrid' {
        $probe = Invoke-SyntheticSearch -ConfigPath $l6Config -Request @{ query = $l6Query; sources = @('codex'); limit = 5 }
        if ($probe.ExitCode -ne 0) { throw "memory_search exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        $response = $probe.Stdout | ConvertFrom-Json
        $source = Find-SourceStatus $response 'codex'
        if (-not $source) { throw 'source_status 缺 codex' }
        if ("$($source.detail.resolved_mode)" -ne 'hybrid') { throw "resolved_mode=$($source.detail.resolved_mode)（期望 hybrid）" }
        if (@($response.results).Count -lt 1) { throw '缺省模式没有返回命中' }
        "resolved_mode=hybrid；命中 $(@($response.results).Count) 条"
    }

    Check '全文索引：exact 模式命中合成文本' {
        $probe = Invoke-SyntheticSearch -ConfigPath $l6Config -Request @{ query = $l6Query; mode = 'exact'; sources = @('codex'); limit = 5 }
        if ($probe.ExitCode -ne 0) { throw "exact exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        $response = $probe.Stdout | ConvertFrom-Json
        if (@($response.results).Count -lt 1) { throw 'exact 模式没有返回命中' }
        $source = Find-SourceStatus $response 'codex'
        if ("$($source.detail.resolved_mode)" -ne 'exact') { throw "resolved_mode=$($source.detail.resolved_mode)（期望 exact）" }
        "命中 $(@($response.results).Count) 条"
    }

    Check '全文索引：缺省模式带 since 不报错' {
        $probe = Invoke-SyntheticSearch -ConfigPath $l6Config -Request @{ query = $l6Query; since = '2026-10-25T00:00:00Z'; sources = @('codex'); limit = 5 }
        if ($probe.ExitCode -ne 0) { throw "带 since 的缺省模式 exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        $response = $probe.Stdout | ConvertFrom-Json
        if (@($response.errors).Count -gt 0) { throw "带 since 的缺省模式返回 errors：$($probe.Stdout)" }
        $source = Find-SourceStatus $response 'codex'
        if ("$($source.detail.resolved_mode)" -ne 'hybrid') { throw "resolved_mode=$($source.detail.resolved_mode)（期望 hybrid）" }
        "resolved_mode=hybrid；errors=0"
    }

    Check '全文索引：删除缓存后缺省模式回落旧路径' {
        Remove-Item -LiteralPath $l6Cache -Recurse -Force
        $probe = Invoke-SyntheticSearch -ConfigPath $l6Config -Request @{ query = $l6Query; sources = @('codex'); limit = 5 }
        if ($probe.ExitCode -ne 0) { throw "删除缓存后 exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        $response = $probe.Stdout | ConvertFrom-Json
        if (@($response.errors).Count -gt 0) { throw "删除缓存后返回 errors：$($probe.Stdout)" }
        $source = Find-SourceStatus $response 'codex'
        $mode = "$($source.detail.resolved_mode)"
        if ($mode -and $mode -ne 'legacy') { throw "删除缓存后 resolved_mode=$mode（期望 legacy/缺省）" }
        if (@($response.results).Count -lt 1) { throw '删除缓存后旧路径没有返回命中' }
        "resolved_mode=$(if ($mode) { $mode } else { '未标注' })；旧路径命中 $(@($response.results).Count) 条"
    }

    $script:recordsHashBefore = $null
    $script:objectCountBefore = 0

    Check '运行时数据可直接对账（记录索引 + 归档对象）' {
        $recordsPath = Get-RecordsJson -RuntimeRoot $runtimeA
        if (-not (Test-Path -LiteralPath $recordsPath -PathType Leaf)) { throw '缺 state\records.json（采集未提交？）' }
        $script:recordsHashBefore = Get-AgentMemoryFileSha256 -Path $recordsPath
        $script:objectCountBefore = @(Get-ChildItem -LiteralPath (Join-Path $runtimeA 'archive\objects') -Recurse -File -Force).Count
        "records.json $($script:recordsHashBefore.Substring(0, 12))…；归档对象 $($script:objectCountBefore) 个"
    }

    Check '重复安装保留运行时数据（幂等）' {
        if (-not $script:recordsHashBefore) { throw '前置对账未完成，跳过' }
        if ($script:objectCountBefore -lt 1) { throw '前置条件失败：归档对象为空' }
        $probe = Invoke-Installer -UserRoot $userA
        if ($probe.ExitCode -ne 0) { throw "重复安装 exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        $recordsHashAfter = Get-AgentMemoryFileSha256 -Path (Get-RecordsJson -RuntimeRoot $runtimeA)
        if ($recordsHashAfter -ne $script:recordsHashBefore) { throw '重复安装改写了记录索引' }
        $objectCountAfter = @(Get-ChildItem -LiteralPath (Join-Path $runtimeA 'archive\objects') -Recurse -File -Force).Count
        if ($objectCountAfter -ne $script:objectCountBefore) { throw "重复安装改变了归档对象数：$($script:objectCountBefore) -> $objectCountAfter" }
        "对象 $objectCountAfter 个、记录索引哈希未变"
    }

    Check '-CheckOnly 在已安装环境返回 0' {
        $probe = Invoke-Installer -UserRoot $userA -Extra @('-CheckOnly')
        if ($probe.ExitCode -ne 0) { throw "CheckOnly exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        'CheckOnly 通过'
    }

    Check '审计 -AgentMemoryOnly 容忍已授权归档数据（-AllowArchivedData）' {
        $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
        if ($probe.ExitCode -ne 0) { throw "audit exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        '容忍已归档的合成数据'
    }

    # --- 反向用例 1：缺失预编译二进制 ---
    $distExe = Join-Path $sandboxDist 'agent-memory.exe'
    $distExeBackup = Join-Path $work 'agent-memory.exe.orig'
    Copy-Item -LiteralPath $distExe -Destination $distExeBackup -Force
    Remove-Item -LiteralPath $distExe -Force
    $userB = Join-Path $work 'userB'

    Check '反向：缺失预编译二进制时安装显式失败' {
        $probe = Invoke-Installer -UserRoot $userB
        Assert-ExitNonZero $probe '缺失二进制安装' | Out-Null
        Assert-FailureMentions $probe 'agent-memory\.exe|预编译二进制' '缺失二进制安装'
        if (Test-Path -LiteralPath (Join-Path $userB '.agent-memory\bin\agent-memory.exe')) { throw '失败后仍留下已安装二进制' }
        '显式失败且未落盘'
    }

    Check '反向：缺失预编译二进制时审计显式 FAIL' {
        $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
        if ($probe.ExitCode -eq 0) { throw '审计未发现缺失的分发物（exit 0）' }
        Assert-FailureMentions $probe 'agent-memory\.exe|预编译二进制' '缺失二进制审计'
        "审计 exit $($probe.ExitCode)"
    }

    # --- 反向用例 2：缺失分发物清单 ---
    $manifestPath = Join-Path $sandboxDist 'build-manifest.json'
    $manifestBackup = Join-Path $work 'build-manifest.json.orig'
    Copy-Item -LiteralPath $manifestPath -Destination $manifestBackup -Force
    Remove-Item -LiteralPath $manifestPath -Force
    $userC = Join-Path $work 'userC'

    Check '反向：缺失分发物清单时安装显式失败' {
        $probe = Invoke-Installer -UserRoot $userC
        Assert-ExitNonZero $probe '缺失清单安装' | Out-Null
        Assert-FailureMentions $probe 'build-manifest\.json|清单' '缺失清单安装'
        '显式失败'
    }

    # --- 反向用例 3：篡改分发物（翻转二进制一个字节） ---
    Copy-Item -LiteralPath $distExeBackup -Destination $distExe -Force
    Copy-Item -LiteralPath $manifestBackup -Destination $manifestPath -Force
    $bytes = [System.IO.File]::ReadAllBytes($distExe)
    $index = $bytes.Length - 128
    $bytes[$index] = $bytes[$index] -bxor 0xFF
    [System.IO.File]::WriteAllBytes($distExe, $bytes)
    $userD = Join-Path $work 'userD'

    Check '反向：篡改分发物后安装显式失败' {
        $probe = Invoke-Installer -UserRoot $userD
        Assert-ExitNonZero $probe '篡改分发物安装' | Out-Null
        Assert-FailureMentions $probe 'SHA256|篡改' '篡改分发物安装'
        if (Test-Path -LiteralPath (Join-Path $userD '.agent-memory\bin\agent-memory.exe')) { throw '失败后仍留下已安装二进制' }
        '显式失败且未落盘'
    }

    Check '反向：篡改分发物后审计显式 FAIL' {
        $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
        if ($probe.ExitCode -eq 0) { throw '审计未发现被篡改的分发物（exit 0）' }
        Assert-FailureMentions $probe 'SHA256|篡改' '篡改分发物审计'
        "审计 exit $($probe.ExitCode)"
    }

    # 复原沙箱分发物，供后续用例使用
    Copy-Item -LiteralPath $distExeBackup -Destination $distExe -Force
    Copy-Item -LiteralPath $manifestBackup -Destination $manifestPath -Force

    Check '反向：源码与分发物不一致时审计显式 FAIL' {
        # 拷贝进来的构建输入被改动（等价于改了源码没重发分发物）
        $cargoPath = Join-Path $sandbox 'agent-memory\Cargo.lock'
        [System.IO.File]::AppendAllText($cargoPath, "`r`n# off-line verification marker`r`n")
        try {
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现源码树与分发物不一致（exit 0）' }
            Assert-FailureMentions $probe '构建输入已变化|treeSha256|树' '源码漂移审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            Copy-Item -LiteralPath (Join-Path $SuiteRoot 'agent-memory\Cargo.lock') -Destination $cargoPath -Force
        }
    }

    Check '凭据扫描不误伤（合法无关词不报）' {
        $localConfig = Join-Path $runtimeA 'state\config.local.json'
        [System.IO.File]::WriteAllText($localConfig, '{"note":"no credential stored; requiresRust false","sources":[]}' + "`r`n", $utf8NoBom)
        try {
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -ne 0) { throw "合法文本被判为凭据（误伤）：$($probe.Stdout)$($probe.Stderr)" }
            '含 credential 字样的配置未误报'
        } finally {
            Remove-Item -LiteralPath $localConfig -Force -ErrorAction SilentlyContinue
        }
    }

    Check '反向：运行时配置文件含凭据时审计 FAIL' {
        # 凭据样本在运行时拼接：交付物文本本身不得出现可被扫描命中的字面量
        $keyName = 'access' + '_token'
        $keyValue = 'value' + 'abcdefghijklmnop'
        $localConfig = Join-Path $runtimeA 'state\config.local.json'
        [System.IO.File]::WriteAllText($localConfig, ('{"providers":{"p1":{"' + $keyName + '":"' + $keyValue + '"}}}') + "`r`n", $utf8NoBom)
        try {
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现 config.local.json 中的凭据（exit 0）' }
            Assert-FailureMentions $probe '凭据|credential' '运行时凭据审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            Remove-Item -LiteralPath $localConfig -Force -ErrorAction SilentlyContinue
        }
    }

    Check '反向：install.json 含凭据时审计 FAIL' {
        $recordPath = Join-Path $userA '.agent-memory\install.json'
        $original = [System.IO.File]::ReadAllText($recordPath, [System.Text.Encoding]::UTF8)
        $propertyName = 'api' + '_key'
        $propertyValue = 'sk-' + ('sample' + 'abcdefghijklmnop')
        try {
            $record = $original | ConvertFrom-Json
            $record | Add-Member -NotePropertyName $propertyName -NotePropertyValue $propertyValue -Force
            [System.IO.File]::WriteAllText($recordPath, (($record | ConvertTo-Json -Depth 6) -replace "`r?`n", "`r`n") + "`r`n", $utf8NoBom)
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现 install.json 中的凭据（exit 0）' }
            Assert-FailureMentions $probe '凭据|credential' '安装元数据凭据审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            [System.IO.File]::WriteAllText($recordPath, $original, $utf8NoBom)
        }
    }

    Check '反向：已安装二进制被删除时审计 FAIL' {
        $exePath = Join-Path $userA '.agent-memory\bin\agent-memory.exe'
        $backup = Join-Path $work 'installed-agent-memory.exe.orig'
        Copy-Item -LiteralPath $exePath -Destination $backup -Force
        try {
            Remove-Item -LiteralPath $exePath -Force
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现缺失的已安装二进制（exit 0）' }
            Assert-FailureMentions $probe '未安装|二进制缺失' '缺失安装审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            if (-not (Test-Path -LiteralPath $exePath)) { Copy-Item -LiteralPath $backup -Destination $exePath -Force }
        }
    }

    Check '反向：已安装二进制被篡改时审计 FAIL' {
        $exePath = Join-Path $userA '.agent-memory\bin\agent-memory.exe'
        $original = [System.IO.File]::ReadAllBytes($exePath)
        try {
            $bytes = [System.IO.File]::ReadAllBytes($exePath)
            $target = $bytes.Length - 64
            $bytes[$target] = $bytes[$target] -bxor 0xFF
            [System.IO.File]::WriteAllBytes($exePath, $bytes)
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现被篡改的已安装二进制（exit 0）' }
            Assert-FailureMentions $probe '哈希不符|SHA256' '安装篡改审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            [System.IO.File]::WriteAllBytes($exePath, $original)
        }
    }

    Check '反向：删除已安装语义辅助脚本后审计 FAIL' {
        $helperPath = Join-Path $userA '.agent-memory\scripts\recall_embedding.py'
        $backup = Join-Path $work 'installed-recall-embedding.py.orig'
        Copy-Item -LiteralPath $helperPath -Destination $backup -Force
        try {
            Remove-Item -LiteralPath $helperPath -Force
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现缺失的语义辅助脚本（exit 0）' }
            Assert-FailureMentions $probe '语义辅助脚本|script_unavailable' '缺失辅助脚本审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            if (-not (Test-Path -LiteralPath $helperPath)) { Copy-Item -LiteralPath $backup -Destination $helperPath -Force }
        }
    }

    Check '反向：篡改已安装语义辅助脚本后审计 FAIL' {
        $helperPath = Join-Path $userA '.agent-memory\scripts\recall_embedding.py'
        $original = [System.IO.File]::ReadAllBytes($helperPath)
        try {
            $bytes = [System.IO.File]::ReadAllBytes($helperPath)
            # 保持文件仍是合法 Python（改一行注释内容），审计仍须按哈希拒绝
            $text = [System.Text.Encoding]::UTF8.GetString($bytes)
            [System.IO.File]::WriteAllText($helperPath, ($text -replace '^"""', '""" tampered'), (New-Object System.Text.UTF8Encoding($false)))
            $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
            if ($probe.ExitCode -eq 0) { throw '审计未发现被篡改的语义辅助脚本（exit 0）' }
            Assert-FailureMentions $probe '哈希不符|SHA256' '辅助脚本篡改审计'
            "审计 exit $($probe.ExitCode)"
        } finally {
            [System.IO.File]::WriteAllBytes($helperPath, $original)
        }
    }

    Check '反向：恢复辅助脚本后审计重新 PASS' {
        $probe = Invoke-AgentMemoryAudit -UserRoot $userA -AllowArchivedData
        if ($probe.ExitCode -ne 0) { throw "恢复后审计仍失败（exit $($probe.ExitCode)）：$($probe.Stdout)$($probe.Stderr)" }
        '删除/篡改用例结束后状态已复原'
    }

    Check '卸载只移除程序目录，运行时数据保留' {
        $probe = Invoke-Installer -UserRoot $userA -Extra @('-Uninstall')
        if ($probe.ExitCode -ne 0) { throw "uninstall exit $($probe.ExitCode)：$($probe.Stdout)$($probe.Stderr)" }
        if (Test-Path -LiteralPath (Join-Path $userA '.agent-memory')) { throw '卸载未移除程序目录' }
        if (Test-Path -LiteralPath (Join-Path $userA '.agent-memory\scripts\recall_embedding.py')) { throw '卸载未移除语义辅助脚本（程序目录内 scripts\）' }
        if (-not (Test-Path -LiteralPath $runtimeA -PathType Container)) { throw '卸载删除了运行时根' }
        if (-not $script:recordsHashBefore) { throw '前置对账未完成' }
        if ((Get-AgentMemoryFileSha256 -Path (Get-RecordsJson -RuntimeRoot $runtimeA)) -ne $script:recordsHashBefore) { throw '卸载改写了运行时数据' }
        '程序目录（含 scripts\）已移除，运行时根与记录索引未变'
    }

    Check '全程未触碰真实运行时根（state/archive 清单 + 哈希前后一致）' {
        if ($SkipRuntimeRootCheck) {
            return 'skipped（-SkipRuntimeRootCheck：机器上有计划采集写入；该项未执行）'
        }
        $existsNow = Test-Path -LiteralPath $realRuntimeRoot
        if ($existsNow -ne $realRuntimeExistedBefore) { throw "真实运行时根存在性发生变化：$realRuntimeExistedBefore -> $existsNow" }
        if (-not $existsNow) { return '根不存在，前后一致' }
        $after = Get-AgentMemoryRuntimeFingerprint -Root $realRuntimeRoot
        if ($null -eq $script:realBefore) { throw '真实运行时根在验收开始时存在，但开始快照缺失' }
        if ($after.Files -ne $script:realBefore.Files) { throw "真实运行时根文件数变化：$($script:realBefore.Files) -> $($after.Files)" }
        if ($after.Digest -ne $script:realBefore.Digest) { throw '真实运行时根 state/archive 内容指纹变化（验收过程改写了既有证据）' }
        "state+archive $($after.Files) 个文件，指纹 $($after.Digest.Substring(0, 12))… 前后一致"
    }
} catch {
    $results.Add([pscustomobject]@{ Item = '验收流程未跑完'; Result = 'FAIL'; Detail = $_.Exception.Message })
} finally {
    $env:Path = $savedPath
    foreach ($key in $proxyKeys) { [System.Environment]::SetEnvironmentVariable($key, $savedProxies[$key], 'Process') }
    if (-not $KeepTemp) { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
    else { Write-Host "临时目录保留：$work" }
}

Write-Host ''
Write-Host '===== agent-memory 离线验收结果 ====='
$results | Format-Table -AutoSize | Out-String | Write-Host
$failCount = @($results | Where-Object { $_.Result -eq 'FAIL' }).Count
if ($failCount -gt 0) {
    Write-Host "$failCount 项未通过；按 Detail 排查（参考 assets\references\agent-memory-archive.md）"
    exit 1
}
Write-Host "全部 $($results.Count) 项通过（临时环境，无真实会话数据）"
exit 0
