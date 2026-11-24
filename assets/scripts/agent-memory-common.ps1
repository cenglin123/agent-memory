# agent-memory-common.ps1 — agent-memory 分发物的共享校验函数（由 61 / 62 / 90 与构建脚本点源）
# 单一实现点：分发物清单读取与内容校验、源码树哈希、PE 头部、GUI 依赖扫描、空运行时根检查。
# 只读：本文件不写任何文件，不联网，不读取真实会话目录。
$script:AgentMemoryDistSchema = 'agent-memory/dist-manifest@1'
$script:AgentMemoryTarget = 'x86_64-pc-windows-msvc'
$script:AgentMemoryBinaryName = 'agent-memory.exe'
# 桌面/图形栈 DLL：出现任一即证明二进制链接了 GUI 依赖（P6 要求无 GUI 依赖）
$script:AgentMemoryGuiDlls = @(
    'user32.dll', 'gdi32.dll', 'dwmapi.dll', 'comctl32.dll', 'shell32.dll',
    'ole32.dll', 'oleaut32.dll', 'd3d11.dll', 'dxgi.dll', 'uxtheme.dll'
)
# 动态 CRT：命中即说明目标机需要 VC++ 运行库（静态 CRT 构建不出现这些导入）
$script:AgentMemoryDynamicCrtPattern = '^(vcruntime\d+\.dll|msvcp\d+\.dll|ucrtbase\.dll|concrt\d+\.dll|api-ms-win-crt-.+\.dll)$'

# `%TEMP%` (or any caller-supplied root) can be an 8.3 short path while
# Get-ChildItem returns long paths; a length-based Substring would then cut
# the root in the middle. Resolve once, at the entry of every path helper.
function Resolve-AgentMemoryLongPath {
    param([Parameter(Mandatory)][string]$Path)
    try {
        if (Test-Path -LiteralPath $Path) { return (Get-Item -LiteralPath $Path).FullName }
    } catch {
        # Fall through: GetFullPath still normalises the spelling.
    }
    return [System.IO.Path]::GetFullPath($Path)
}

function Get-AgentMemoryFileSha256 {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "文件缺失：$Path" }
    $stream = [System.IO.File]::OpenRead($Path)
    try {
        $sha = [System.Security.Cryptography.SHA256]::Create()
        try { $hash = $sha.ComputeHash($stream) } finally { $sha.Dispose() }
    } finally { $stream.Dispose() }
    return ([System.BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}

# 构建输入 = 会改变发布二进制的文件（Cargo.toml / 锁文件 / 工具链钉定 / 构建脚本 / src 全部文件）。
# tests 不参与：测试改动不改变二进制，纳入会让分发物与源码无故失配。
function Get-AgentMemorySourceFiles {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    $crateRoot = Resolve-AgentMemoryLongPath -Path (Join-Path $SuiteRoot 'agent-memory')
    $files = [System.Collections.Generic.List[string]]::new()
    foreach ($rel in @('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'build/build-release.ps1')) {
        if (-not (Test-Path -LiteralPath (Join-Path $crateRoot $rel) -PathType Leaf)) { throw "构建输入缺失：agent-memory/$rel" }
        $files.Add("agent-memory/$rel")
    }
    $srcRoot = Join-Path $crateRoot 'src'
    if (-not (Test-Path -LiteralPath $srcRoot -PathType Container)) { throw '构建输入缺失：agent-memory/src' }
    foreach ($file in Get-ChildItem -LiteralPath $srcRoot -Recurse -File -Force -ErrorAction Stop) {
        if ($file.Name.StartsWith('.')) { continue }
        $rel = $file.FullName.Substring($crateRoot.Length + 1) -replace '\\', '/'
        $files.Add("agent-memory/$rel")
    }
    return $files.ToArray()
}

# 源码树哈希：按相对路径序（序号排序）取「路径 \0 内容 \0」喂 SHA-256，内容先把 CRLF 归一为 LF。
# 换行归一让同一提交在 autocrlf 检出（CRLF）与 worktree / 其他设备（LF）上得到同一哈希；
# rustc 读源码时同样归一 CRLF，换行差异不改变发布二进制。
# 归一用 ISO-8859-1（码点与字节一一对应）往返，字节级精确，非文本字节原样保留。
function Get-AgentMemorySourceTreeHash {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    $paths = Get-AgentMemorySourceFiles -SuiteRoot $SuiteRoot
    [System.Array]::Sort($paths, [System.StringComparer]::Ordinal)
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try {
        $buffer = [System.IO.MemoryStream]::new()
        try {
            foreach ($relative in $paths) {
                $full = Join-Path $SuiteRoot ($relative -replace '/', '\')
                $name = [System.Text.Encoding]::UTF8.GetBytes($relative)
                $body = $latin1.GetBytes($latin1.GetString([System.IO.File]::ReadAllBytes($full)).Replace("`r`n", "`n"))
                $buffer.Write($name, 0, $name.Length)
                $buffer.WriteByte(0)
                $buffer.Write($body, 0, $body.Length)
                $buffer.WriteByte(0)
            }
            $hash = $sha.ComputeHash($buffer.ToArray())
        } finally { $buffer.Dispose() }
    } finally { $sha.Dispose() }
    return ([System.BitConverter]::ToString($hash) -replace '-', '').ToLowerInvariant()
}

function Get-AgentMemoryDistDir {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    return (Join-Path $SuiteRoot 'assets\dist\agent-memory')
}

# 语义辅助脚本（配置 embedding 且 backend=python 时使用）：二进制从 exe 目录及其祖先查找
# <ancestor>\scripts\recall_embedding.py（次选 <ancestor>\assets\scripts\），见
# agent-memory\src\recall\semantic\embed.rs resolve_script。安装位置必须命中该查找，
# 否则生产安装下 Archive 语义检索恒为 script_unavailable。
# 单一实现点：来源路径、安装路径、期望哈希都由本文件给出，61 安装与 90 审计共用。
# 来源 = 套件 assets\scripts（与 61/90 本身同属套件脚本，随套件目录分发，不复制进
# assets\dist —— dist 只承载需 Rust 工具链构建的二进制；脚本身份由 Git 与哈希核对保证）。
$script:AgentMemoryHelperScriptName = 'recall_embedding.py'
$script:AgentMemoryHelperScriptRelPath = 'assets\scripts\recall_embedding.py'

function Get-AgentMemoryHelperScriptSourcePath {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    return (Join-Path $SuiteRoot $script:AgentMemoryHelperScriptRelPath)
}

function Get-AgentMemoryHelperScriptInstallPath {
    param([Parameter(Mandatory)][string]$UserRoot)
    return (Join-Path $UserRoot ('.agent-memory\scripts\' + $script:AgentMemoryHelperScriptName))
}

# 期望哈希 = 套件源文件哈希（唯一事实源）。安装与审计都按它核对已安装副本：
# 脚本缺失、被改、或安装的是别的版本，一律显式失败而不是静默降级。
function Get-AgentMemoryHelperScriptExpectation {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    $sourcePath = Get-AgentMemoryHelperScriptSourcePath -SuiteRoot $SuiteRoot
    if (-not (Test-Path -LiteralPath $sourcePath -PathType Leaf)) {
        throw "语义辅助脚本来源缺失：$sourcePath（套件不完整；同步 assets\scripts 后重试）"
    }
    return [pscustomobject]@{
        Path = $sourcePath
        Sha256 = (Get-AgentMemoryFileSha256 -Path $sourcePath)
    }
}

# 工具链钉定单一来源：agent-memory\rust-toolchain.toml（channel + targets）。
# 构建脚本、安装/审计校验都从这里读，不在脚本里另写版本字面量。
function Get-AgentMemoryPinnedToolchain {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    $path = Join-Path $SuiteRoot 'agent-memory\rust-toolchain.toml'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "工具链钉定缺失：$path" }
    $text = [System.IO.File]::ReadAllText($path)
    if ($text -notmatch 'channel\s*=\s*"([^"]+)"') { throw "rust-toolchain.toml 缺 channel：$path" }
    $channel = $Matches[1]
    if ($text -notmatch 'targets\s*=\s*\[([^\]]*)\]') { throw "rust-toolchain.toml 缺 targets：$path" }
    $targets = @([regex]::Matches($Matches[1], '"([^"]+)"') | ForEach-Object { $_.Groups[1].Value })
    return [pscustomobject]@{ Path = $path; Channel = $channel; Targets = $targets }
}

# 读取并严格校验 build-manifest.json（字段、钉定 target、哈希形状、谱系自洽）。
# 缺失或形状不合法一律抛错：目标机不得现场构建替代品。
# PS 5.1 与 PS 7 的 ConvertFrom-Json 对 ISO 时间分别给 string / [datetime]，因此 builtAtUtc
# 一律从原始 JSON 文本取值并归一，避免类型差异导致误报（PS 7 下曾把 10/25/2026 18:02:16 判为非法）。
function Read-AgentMemoryDistManifest {
    param([Parameter(Mandatory)][string]$DistDir)
    $path = Join-Path $DistDir 'build-manifest.json'
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "预编译分发物清单缺失：$path。套件不完整：离线安装只消费预编译分发物，不得现场安装 Rust 工具链重建（见 assets\references\agent-memory-archive.md）"
    }
    $raw = [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
    try {
        $manifest = $raw | ConvertFrom-Json
    } catch {
        throw "预编译分发物清单不是合法 JSON：$path（$($_.Exception.Message)）"
    }
    $names = @($manifest.PSObject.Properties.Name)
    foreach ($required in @('schema', 'component', 'version', 'target', 'profile', 'binary', 'binarySha256', 'binaryBytes', 'peHeader', 'toolchain', 'build', 'source', 'runtime', 'license')) {
        if ($names -notcontains $required) { throw "预编译分发物清单缺字段 $required：$path" }
    }
    if ($manifest.schema -ne $script:AgentMemoryDistSchema) { throw "分发物清单 schema 非预期：$($manifest.schema)" }
    if ($manifest.component -ne 'agent-memory') { throw "分发物清单 component 非预期：$($manifest.component)" }
    if ($manifest.target -ne $script:AgentMemoryTarget) { throw "分发物 target 必须钉定为 $($script:AgentMemoryTarget)，实际 $($manifest.target)" }
    if ($manifest.profile -ne 'release') { throw "分发物必须是 release 构建，实际 profile $($manifest.profile)" }
    if ($manifest.binary -ne $script:AgentMemoryBinaryName) { throw "分发物二进制名非预期：$($manifest.binary)" }
    if ($manifest.peHeader -ne 'MZ') { throw "分发物清单 peHeader 非 MZ：$($manifest.peHeader)" }
    if ("$($manifest.binarySha256)" -notmatch '^[0-9a-f]{64}$') { throw "分发物清单 binarySha256 非法：$($manifest.binarySha256)" }
    # 谱系自洽：commit 指向"确实包含本次构建输入"的提交（可为 HEAD 的祖先，见 Assert-AgentMemoryLineage），
    # 工作区有未提交改动时为 null（baseCommit 记当前 HEAD 作上下文）。
    if ("$($manifest.source.baseCommit)" -notmatch '^[0-9a-f]{40}$') { throw "分发物清单 source.baseCommit 非 40 位 Git commit：$($manifest.source.baseCommit)" }
    $treeDirtyValue = $manifest.source.treeDirty
    if ($treeDirtyValue -isnot [bool]) { throw "分发物清单 source.treeDirty 必须是布尔值：$treeDirtyValue" }
    if ($treeDirtyValue) {
        if ($null -ne $manifest.source.commit) { throw "分发物清单 source.treeDirty=true 时 source.commit 必须为 null（不得指向不含构建输入的提交）" }
    } else {
        if ("$($manifest.source.commit)" -notmatch '^[0-9a-f]{40}$') { throw "分发物清单 source.commit 非 40 位 Git commit：$($manifest.source.commit)" }
    }
    if ("$($manifest.source.treeSha256)" -notmatch '^[0-9a-f]{64}$') { throw "分发物清单 source.treeSha256 非法：$($manifest.source.treeSha256)" }
    if ("$($manifest.toolchain.pinnedChannel)" -notmatch '^\d+\.\d+\.\d+$') { throw "工具链 channel 非法：$($manifest.toolchain.pinnedChannel)" }
    if ("$($manifest.toolchain.rustcVersion)" -notmatch ('^rustc ' + [regex]::Escape("$($manifest.toolchain.pinnedChannel)") + ' \(')) { throw "分发物 rustc 版本与钉定 channel 不符：$($manifest.toolchain.rustcVersion) / $($manifest.toolchain.pinnedChannel)" }
    if (-not $manifest.toolchain.locked) { throw '分发物必须由 --locked 构建（锁文件生效）' }
    if (-not $manifest.toolchain.crtStatic) { throw '分发物必须用静态 CRT 构建（RUSTFLAGS=-C target-feature=+crt-static），否则目标机需要 VC++ 运行库' }
    if ("$($manifest.build.rustflags)" -notmatch 'crt-static') { throw "分发物清单 build.rustflags 未记录 crt-static：$($manifest.build.rustflags)" }
    if ($manifest.build.script -ne 'agent-memory/build/build-release.ps1') { throw "分发物构建脚本记录非预期：$($manifest.build.script)" }
    $builtAtText = "$($manifest.build.builtAtUtc)"
    if ($builtAtText -notmatch '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$') {
        $rawMatch = [regex]::Match($raw, '"builtAtUtc"\s*:\s*"([^"]+)"')
        if (-not $rawMatch.Success) { throw "分发物构建时间无法取值：$builtAtText" }
        $builtAtText = $rawMatch.Groups[1].Value
    }
    if ($builtAtText -notmatch '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z$') { throw "分发物构建时间格式非预期：$builtAtText" }
    $manifest.build.builtAtUtc = $builtAtText
    if (@($manifest.runtime.guiDependencies).Count -ne 0) { throw "分发物清单声明了 GUI 依赖：$(@($manifest.runtime.guiDependencies) -join ', ')" }
    if ($manifest.runtime.requiresRust -or $manifest.runtime.requiresPython -or $manifest.runtime.requiresNode) { throw '分发物声明了运行时语言依赖；目标机只允许消费预编译二进制' }
    if ("$($manifest.license.notice)" -ne 'NOTICE') { throw "分发物必须随行 cc-switch NOTICE，清单记录 $($manifest.license.notice)" }
    return $manifest
}

# 内容校验：清单 ↔ 分发物 exe ↔ 随行许可文件 + 二进制导入表（无 GUI / 无动态 CRT）。
# 任一不符即抛错（反向用例：篡改分发物必报）。
function Assert-AgentMemoryDistIntact {
    param(
        [Parameter(Mandatory)][string]$SuiteRoot,
        [string]$DistDir
    )
    if (-not $DistDir) { $DistDir = Get-AgentMemoryDistDir -SuiteRoot $SuiteRoot }
    $manifest = Read-AgentMemoryDistManifest -DistDir $DistDir
    $exe = Join-Path $DistDir $manifest.binary
    if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) {
        throw "预编译二进制缺失：$exe。离线安装必须显式失败而不是现场构建（见 assets\references\agent-memory-archive.md）"
    }
    $actual = Get-AgentMemoryFileSha256 -Path $exe
    if ($actual -ne $manifest.binarySha256) {
        throw "分发物被篡改或清单过期：$exe 实际 SHA256 $actual，清单记录 $($manifest.binarySha256)"
    }
    $size = (Get-Item -LiteralPath $exe).Length
    if ($size -ne [int64]$manifest.binaryBytes) { throw "分发物大小与清单不符：实际 $size 字节，清单 $($manifest.binaryBytes)" }
    if ($manifest.peHeader -ne 'MZ') { throw "分发物不是 PE 可执行文件（文件头非 MZ）：$exe" }
    $hardened = Assert-AgentMemoryBinaryHardened -Path $exe
    foreach ($name in @($manifest.license.notice, $manifest.license.license)) {
        $distCopy = Join-Path $DistDir $name
        $sourceCopy = Join-Path $SuiteRoot "agent-memory\$name"
        if (-not (Test-Path -LiteralPath $distCopy -PathType Leaf)) { throw "分发物缺随行许可文件：$distCopy" }
        if (-not (Test-Path -LiteralPath $sourceCopy -PathType Leaf)) { throw "许可来源缺失：$sourceCopy" }
        if ((Get-AgentMemoryFileSha256 -Path $distCopy) -ne (Get-AgentMemoryFileSha256 -Path $sourceCopy)) {
            throw "分发物随行许可文件与 agent-memory\$name 不一致（来源声明必须随二进制保留）"
        }
    }
    return [pscustomobject]@{ Manifest = $manifest; ExePath = $exe; Sha256 = $actual; Imports = $hardened.Imports }
}

# PE 导入表解析（读 IMAGE_IMPORT_DESCRIPTOR 的 DLL 名），取代按字节 ASCII 猜测：
# 只有真正链接的 DLL 才算依赖，文档/字符串常量里的同名文本不会误报。
function Get-AgentMemoryPeMetadata {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "文件缺失：$Path" }
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 0x40 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) { throw "不是 PE 文件（文件头非 MZ）：$Path" }
    $u16 = { param([int]$offset) [System.BitConverter]::ToUInt16($bytes, $offset) }
    $u32 = { param([int]$offset) [System.BitConverter]::ToUInt32($bytes, $offset) }
    $ascii = {
        param([int]$offset)
        $end = $offset
        while ($end -lt $bytes.Length -and $bytes[$end] -ne 0) { $end++ }
        [System.Text.Encoding]::ASCII.GetString($bytes, $offset, $end - $offset)
    }
    $peOffset = [int](& $u32 0x3C)
    if ($peOffset -le 0 -or $peOffset + 24 -ge $bytes.Length) { throw "PE 头偏移非法：$peOffset" }
    if ((& $ascii $peOffset) -ne 'PE') { throw "PE 签名缺失：$Path" }
    $coff = $peOffset + 4
    $machine = & $u16 $coff
    $sectionCount = [int](& $u16 ($coff + 2))
    $optionalSize = [int](& $u16 ($coff + 16))
    $optional = $coff + 20
    $magic = & $u16 $optional
    if ($magic -eq 0x20B) { $dataDirectory = $optional + 112 }
    elseif ($magic -eq 0x10B) { $dataDirectory = $optional + 96 }
    else { throw ("未知可选头 Magic 0x{0:X4}：$Path" -f $magic) }
    $subsystem = & $u16 ($optional + 68)
    $importRva = [int](& $u32 ($dataDirectory + 8))
    $sections = @()
    $sectionStart = $optional + $optionalSize
    for ($i = 0; $i -lt $sectionCount; $i++) {
        $base = $sectionStart + 40 * $i
        $virtualSize = [int](& $u32 ($base + 8))
        $virtualAddress = [int](& $u32 ($base + 12))
        $rawSize = [int](& $u32 ($base + 16))
        $rawPointer = [int](& $u32 ($base + 20))
        $sections += [pscustomobject]@{
            VirtualAddress = $virtualAddress
            Span = [Math]::Max($virtualSize, $rawSize)
            RawPointer = $rawPointer
        }
    }
    $rvaToOffset = {
        param([int]$rva)
        foreach ($section in $sections) {
            if ($rva -ge $section.VirtualAddress -and $rva -lt ($section.VirtualAddress + $section.Span)) {
                return $rva - $section.VirtualAddress + $section.RawPointer
            }
        }
        return -1
    }
    $imports = @()
    if ($importRva -ne 0) {
        $cursor = & $rvaToOffset $importRva
        if ($cursor -lt 0) { throw ("导入表 RVA 无对应节：0x{0:X}（{1}）" -f $importRva, $Path) }
        while ($cursor + 20 -le $bytes.Length) {
            $originalThunk = & $u32 $cursor
            $nameRva = [int](& $u32 ($cursor + 12))
            $firstThunk = & $u32 ($cursor + 16)
            if ($originalThunk -eq 0 -and $nameRva -eq 0 -and $firstThunk -eq 0) { break }
            if ($nameRva -eq 0) { break }
            $nameOffset = & $rvaToOffset $nameRva
            if ($nameOffset -lt 0) { throw ("导入 DLL 名 RVA 无对应节：0x{0:X}（{1}）" -f $nameRva, $Path) }
            $imports += (& $ascii $nameOffset)
            $cursor += 20
        }
    }
    if ($imports.Count -eq 0) { throw "未解析到任何导入 DLL（PE 结构异常或解析失败）：$Path" }
    return [pscustomobject]@{
        Path = $Path
        Machine = $machine
        Magic = $magic
        Subsystem = $subsystem
        ImportRva = $importRva
        Imports = @($imports | Sort-Object -Unique)
    }
}

# 二进制硬化检查：无桌面 GUI 栈、无动态 CRT（否则目标机需要 VC++ 运行库）。
function Assert-AgentMemoryBinaryHardened {
    param([Parameter(Mandatory)][string]$Path)
    $metadata = Get-AgentMemoryPeMetadata -Path $Path
    $gui = @($metadata.Imports | Where-Object { $script:AgentMemoryGuiDlls -contains $_.ToLowerInvariant() })
    if ($gui.Count -gt 0) { throw "二进制链接了 GUI 依赖：$($gui -join ', ')（P6 要求无 GUI 依赖）" }
    $dynamicCrt = @($metadata.Imports | Where-Object { $_.ToLowerInvariant() -match $script:AgentMemoryDynamicCrtPattern })
    if ($dynamicCrt.Count -gt 0) {
        throw "二进制链接了动态 CRT：$($dynamicCrt -join ', ')。必须用 RUSTFLAGS=-C target-feature=+crt-static 重建（否则目标机需 VC++ 运行库）"
    }
    return [pscustomobject]@{ Imports = $metadata.Imports; GuiHits = $gui; DynamicCrtHits = $dynamicCrt; Metadata = $metadata }
}

function Get-AgentMemoryGuiDependencyHits {
    param([Parameter(Mandatory)][string]$Path)
    return @((Get-AgentMemoryPeMetadata -Path $Path).Imports | Where-Object { $script:AgentMemoryGuiDlls -contains $_.ToLowerInvariant() })
}

# 仓库副本内的谱系现状：HEAD + 构建输入的未提交改动数。非 git 副本返回 Available=$false（目标机多为整目录拷贝）。
function Get-AgentMemoryBuildScopes {
    # 与 Get-AgentMemorySourceFiles 的构建输入范围一致（目录形式给 git pathspec 用）
    return @(
        'agent-memory/Cargo.toml', 'agent-memory/Cargo.lock', 'agent-memory/rust-toolchain.toml',
        'agent-memory/build', 'agent-memory/src'
    )
}

function Get-AgentMemoryGitLineage {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    if (-not (Test-Path -LiteralPath (Join-Path $SuiteRoot '.git'))) { return [pscustomobject]@{ Available = $false } }
    $head = (& cmd /c "git -C `"$SuiteRoot`" rev-parse HEAD 2>&1" | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $head -notmatch '^[0-9a-f]{40}$') { return [pscustomobject]@{ Available = $false } }
    $scopeArgs = ((Get-AgentMemoryBuildScopes) | ForEach-Object { '"' + $_ + '"' }) -join ' '
    $lines = @((& cmd /c "git -C `"$SuiteRoot`" status --porcelain -- $scopeArgs 2>&1") | Where-Object { $_ -and $_.Trim() })
    return [pscustomobject]@{ Available = $true; Head = $head; DirtyCount = $lines.Count; DirtyLines = $lines }
}

# 提交是否存在且是 HEAD 的祖先：'ancestor' | 'not-ancestor' | 'missing'。
# 判据不用"commit == HEAD"：写入清单的提交必须再提交一次，HEAD 必然变化，字符串相等对已提交清单不可满足。
function Test-AgentMemoryCommitAncestry {
    param(
        [Parameter(Mandatory)][string]$SuiteRoot,
        [Parameter(Mandatory)][string]$Commit,
        [Parameter(Mandatory)][string]$Head
    )
    & cmd /c "git -C `"$SuiteRoot`" cat-file -e `"$Commit^{commit}`" 2>&1" | Out-Null
    if ($LASTEXITCODE -ne 0) { return 'missing' }
    & cmd /c "git -C `"$SuiteRoot`" merge-base --is-ancestor `"$Commit`" `"$Head`" 2>&1" | Out-Null
    if ($LASTEXITCODE -eq 0) { return 'ancestor' }
    return 'not-ancestor'
}

# 该提交的构建输入内容是否与当前工作区逐字节一致（内容判据，不是字符串判据）。
function Test-AgentMemoryCommitContainsInputs {
    param(
        [Parameter(Mandatory)][string]$SuiteRoot,
        [Parameter(Mandatory)][string]$Commit
    )
    $scopeArgs = ((Get-AgentMemoryBuildScopes) | ForEach-Object { '"' + $_ + '"' }) -join ' '
    & cmd /c "git -C `"$SuiteRoot`" diff --quiet `"$Commit`" -- $scopeArgs 2>&1" | Out-Null
    return ($LASTEXITCODE -eq 0)
}

# 谱系语义校验（90 与 build -Verify 共用同一实现）：
# - treeDirty=true：工作区必须确有构建输入改动（否则清单声称 dirty 而实际干净 → 过期）；
# - treeDirty=false：工作区不得有构建输入改动；commit 必须存在、是 HEAD 祖先，且其构建输入内容与工作区一致。
# 返回供输出的说明字符串；不满足即抛错。非 git 副本跳过（并明确说明）。
function Assert-AgentMemoryLineage {
    param(
        [Parameter(Mandatory)]$Manifest,
        [Parameter(Mandatory)][string]$SuiteRoot
    )
    $lineage = Get-AgentMemoryGitLineage -SuiteRoot $SuiteRoot
    if (-not $lineage.Available) { return '非 git 副本（整目录拷贝），跳过 commit 祖先核对' }
    $expectDirty = [bool]$Manifest.source.treeDirty
    $actualDirty = ($lineage.DirtyCount -gt 0)
    if ($expectDirty) {
        if (-not $actualDirty) {
            throw "清单谱系过期：清单 treeDirty=true 但工作区构建输入无改动（HEAD $($lineage.Head.Substring(0, 12))…）。运行 build-release.ps1 -RefreshLineage"
        }
        return "commit null（$($Manifest.source.treeDirtyFiles) 处构建输入未提交，base $($Manifest.source.baseCommit.Substring(0, 12))…）"
    }
    if ($actualDirty) {
        throw "清单 treeDirty=false 但工作区有 $($lineage.DirtyCount) 处构建输入改动；要么提交并用 -RefreshLineage 记录，要么重建分发物"
    }
    $ancestry = Test-AgentMemoryCommitAncestry -SuiteRoot $SuiteRoot -Commit $Manifest.source.commit -Head $lineage.Head
    if ($ancestry -eq 'missing') {
        throw "清单 source.commit $($Manifest.source.commit) 在仓库中不存在；运行 build-release.ps1 -RefreshLineage 重新记录"
    }
    if ($ancestry -eq 'not-ancestor') {
        throw "清单 source.commit $($Manifest.source.commit) 不是 HEAD $($lineage.Head) 的祖先（历史被改写或清单来自其他分支）"
    }
    if (-not (Test-AgentMemoryCommitContainsInputs -SuiteRoot $SuiteRoot -Commit $Manifest.source.commit)) {
        throw "清单 source.commit $($Manifest.source.commit.Substring(0, 12))… 的构建输入与当前工作区不一致（改了源码没重发分发物）"
    }
    $relation = if ($Manifest.source.commit -eq $lineage.Head) { 'HEAD' } else { 'HEAD 祖先' }
    return "commit $($Manifest.source.commit.Substring(0, 12))…（$relation）"
}

# P6 交付物（入库/入分发物）固定清单：凭据扫描的覆盖范围，存在即扫。
# 完整套件含 agent-memory README 时，P6 报告必须全部存在；精简验收沙箱不复制这些文档。
function Get-AgentMemoryTrackedDeliverables {
    param([Parameter(Mandatory)][string]$SuiteRoot)
    $paths = @(
        'assets\scripts\00-precheck.ps1',
        'assets\scripts\61-install-agent-memory.ps1',
        'assets\scripts\62-verify-agent-memory-offline.ps1',
        'assets\scripts\63-verify-agent-memory-lineage.ps1',
        'assets\scripts\90-audit.ps1',
        'assets\scripts\agent-memory-common.ps1',
        'assets\dist\agent-memory\build-manifest.json',
        'assets\dist\agent-memory\NOTICE',
        'assets\dist\agent-memory\LICENSE',
        'assets\references\agent-memory-archive.md',
        'assets\references\conventions.md',
        'agent-memory\Cargo.toml',
        'agent-memory\Cargo.lock',
        'agent-memory\rust-toolchain.toml',
        'agent-memory\NOTICE',
        'agent-memory\LICENSE',
        'agent-memory\README.md',
        'agent-memory\build\build-release.ps1',
        'agent-memory\docs\verification\P6-ARCHIVE-COMPLETED.md',
        'agent-memory\docs\verification\P6-FIX-COMPLETED.md',
        'agent-memory\docs\verification\P6-LINEAGE-FIXED.md',
        'agent-memory\tests\p6_cli_surface.rs',
        'README.md',
        'INSTALL.md',
        'AGENTS.md',
        'CHANGELOG.md',
        'CURRENT.md'
    )
    if (Test-Path -LiteralPath (Join-Path $SuiteRoot 'agent-memory\README.md') -PathType Leaf) {
        foreach ($relative in $paths | Where-Object { $_ -like 'agent-memory\docs\verification\P6-*.md' }) {
            if (-not (Test-Path -LiteralPath (Join-Path $SuiteRoot $relative) -PathType Leaf)) {
                throw "完整套件缺少 P6 验收文档：$relative"
            }
        }
    }
    $result = @()
    foreach ($relative in $paths) {
        $full = Join-Path $SuiteRoot $relative
        if (Test-Path -LiteralPath $full -PathType Leaf) { $result += $full }
    }
    $srcRoot = Join-Path $SuiteRoot 'agent-memory\src'
    if (Test-Path -LiteralPath $srcRoot -PathType Container) {
        $result += @(Get-ChildItem -LiteralPath $srcRoot -Recurse -File -Force -ErrorAction Stop | ForEach-Object { $_.FullName })
    }
    return $result
}

# 凭据模式规则（单一来源）：90 的交付物文本扫描（Get-AgentMemoryCredentialHits）与运行时根
# 对象扫描（agent-memory audit-scan）共用同一组规则。只匹配"字段名 + 赋值形态"和已知密钥前缀，
# 文档里出现的裸词（如 "credential"、"token"）不算命中。
function Get-AgentMemoryCredentialRules {
    return @(
        @{ Name = 'assigned-secret'; Pattern = '(?i)\b(api[_-]?key|secret|client[_-]?secret|password|passwd|credential|access[_-]?token|refresh[_-]?token|auth[_-]?token|private[_-]?key|session[_-]?key|bearer)\b["'']?\s*[:=]\s*["'']?([A-Za-z0-9_\-\.\/\+]{12,})' },
        @{ Name = 'known-key-prefix'; Pattern = '(?:sk-[A-Za-z0-9]{16,}|ghp_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|xox[baprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16})' },
        @{ Name = 'private-key-block'; Pattern = '-----BEGIN [A-Z ]*PRIVATE KEY-----' },
        @{ Name = 'jwt'; Pattern = '\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}' }
    )
}

# 凭据模式扫描：返回命中项（路径/行号/模式名，不复制密钥原文）。
function Get-AgentMemoryCredentialHits {
    param(
        [Parameter(Mandatory)][string[]]$Path,
        [string]$SuiteRoot
    )
    $rules = Get-AgentMemoryCredentialRules
    if ($SuiteRoot) { $SuiteRoot = Resolve-AgentMemoryLongPath -Path $SuiteRoot }
    $hits = @()
    foreach ($file in $Path) {
        if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { continue }
        $display = $file
        if ($SuiteRoot -and $file.StartsWith($SuiteRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
            $display = $file.Substring($SuiteRoot.Length).TrimStart('\')
        }
        $lineNumber = 0
        foreach ($line in [System.IO.File]::ReadLines($file, [System.Text.Encoding]::UTF8)) {
            $lineNumber++
            foreach ($rule in $rules) {
                if ([regex]::IsMatch($line, $rule.Pattern)) {
                    $hits += [pscustomobject]@{
                        Path = $display
                        Line = $lineNumber
                        Rule = $rule.Name
                        Excerpt = ($line.Trim() -replace '(?i)([:=]\s*["'']?)[A-Za-z0-9_\-\.\/\+]{12,}', '$1<redacted>')
                    }
                    break
                }
            }
        }
    }
    return $hits
}

# 参数拼接：PS 5.1 的 Start-Process -ArgumentList 数组按空格拼接、不加引号；
# 含空格的路径（用户目录常见）必须自行加引号。构建/安装/审计共用同一实现。
function Format-AgentMemoryArgumentLine {
    param([string[]]$Arguments)
    if (-not $Arguments) { return '' }
    return (($Arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' ')
}

# 运行进程并返回 { ExitCode; Stdout; Stderr }；不依赖 PATH（只认显式路径）。
function Invoke-AgentMemoryProcess {
    param(
        [Parameter(Mandatory)][string]$ExePath,
        [string[]]$Arguments
    )
    $argLine = Format-AgentMemoryArgumentLine -Arguments $Arguments
    # .NET Process instead of Start-Process -Wait -PassThru: on Windows PowerShell
    # 5.1 the latter throws "Process has exited, so the requested information is
    # not available" while reading ExitCode when a short-lived child exits fast,
    # which surfaced as spurious install/audit failures.
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ExePath
    $psi.Arguments = $argLine
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::Start($psi)
    try {
        # Read raw bytes: the CLI writes UTF-8, while a redirected PowerShell
        # child (build/audit scripts) writes the OEM code page. Decoding is
        # decided per stream below instead of forcing one encoding on both.
        $outBuffer = New-Object System.IO.MemoryStream
        $errBuffer = New-Object System.IO.MemoryStream
        $outTask = $process.StandardOutput.BaseStream.CopyToAsync($outBuffer)
        $errTask = $process.StandardError.BaseStream.CopyToAsync($errBuffer)
        $process.WaitForExit()
        $exitCode = $process.ExitCode
        $outTask.GetAwaiter().GetResult() | Out-Null
        $errTask.GetAwaiter().GetResult() | Out-Null
        $stdoutText = Convert-AgentMemoryChildOutput -Bytes $outBuffer.ToArray()
        $stderrText = Convert-AgentMemoryChildOutput -Bytes $errBuffer.ToArray()
        return [pscustomobject]@{
            ExitCode = $exitCode
            Stdout = $stdoutText
            Stderr = $stderrText
        }
    } finally {
        $process.Dispose()
    }
}

# Strict UTF-8 first (the CLI and the suite's own scripts set UTF-8 output);
# fall back to the OEM code page for a redirected child that did not.
function Convert-AgentMemoryChildOutput {
    param([byte[]]$Bytes)
    if ($null -eq $Bytes -or $Bytes.Length -eq 0) { return '' }
    try {
        $strict = New-Object System.Text.UTF8Encoding($false, $true)
        $text = $strict.GetString($Bytes)
        # Raw-byte decoding does not strip a BOM the way StreamReader does.
        if ($text.Length -gt 0 -and $text[0] -eq [char]0xFEFF) { return $text.Substring(1) }
        return $text
    } catch {
        # A redirected PowerShell child writes the OEM code page, not the ANSI
        # one; both are tried because the two differ on non-CJK systems.
        try {
            $oem = [System.Globalization.CultureInfo]::CurrentCulture.TextInfo.OEMCodePage
            return [System.Text.Encoding]::GetEncoding($oem).GetString($Bytes)
        } catch {
            try {
                return [System.Text.Encoding]::GetEncoding(0).GetString($Bytes)
            } catch {
                return [System.Text.Encoding]::Default.GetString($Bytes)
            }
        }
    }
}

function Invoke-AgentMemoryCli {
    param(
        [Parameter(Mandatory)][string]$ExePath,
        [Parameter(Mandatory)][string[]]$Arguments
    )
    return Invoke-AgentMemoryProcess -ExePath $ExePath -Arguments $Arguments
}

# 运行时根只允许 init 建立的空骨架；出现会话数据、配置或未知条目即视为越界（P6 数据边界）。
function Test-AgentMemoryRuntimeRootEmpty {
    param([Parameter(Mandatory)][string]$RuntimeRoot)
    $problems = [System.Collections.Generic.List[string]]::new()
    $allowedDirs = @('archive', 'derived', 'locks', 'logs', 'state')
    $allowedFiles = @('state\schema.json', 'state\device_id')
    if (-not (Test-Path -LiteralPath $RuntimeRoot -PathType Container)) {
        $problems.Add("运行时根不存在：$RuntimeRoot")
        return [pscustomobject]@{ Ok = $false; Problems = $problems.ToArray(); Records = 0 }
    }
    foreach ($entry in Get-ChildItem -LiteralPath $RuntimeRoot -Force -ErrorAction Stop) {
        if ($entry.PSIsContainer -and $allowedDirs -contains $entry.Name) { continue }
        if (-not $entry.PSIsContainer -and $allowedFiles -contains $entry.Name) { continue }
        $problems.Add("运行时根出现未知条目：$($entry.Name)")
    }
    $records = 0
    $recordsPath = Join-Path $RuntimeRoot 'state\records.json'
    if (Test-Path -LiteralPath $recordsPath -PathType Leaf) {
        try {
            $parsed = [System.IO.File]::ReadAllText($recordsPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
            $records = @($parsed.records.PSObject.Properties).Count
        } catch { $problems.Add('state\records.json 无法解析') }
    }
    $sourcesPath = Join-Path $RuntimeRoot 'state\sources.json'
    if (Test-Path -LiteralPath $sourcesPath -PathType Leaf) {
        try {
            $parsed = [System.IO.File]::ReadAllText($sourcesPath, [System.Text.Encoding]::UTF8) | ConvertFrom-Json
            $sourceCount = @($parsed.sources.PSObject.Properties).Count
            if ($sourceCount -gt 0) { $problems.Add("state\sources.json 已登记 $sourceCount 个数据源；P6 不得自动采集") }
        } catch { $problems.Add('state\sources.json 无法解析') }
    }
    if (Test-Path -LiteralPath (Join-Path $RuntimeRoot 'config.local.json') -PathType Leaf) {
        $problems.Add('运行时根存在 config.local.json（安装期不得自动生成本机数据源配置）')
    }
    foreach ($dir in @('archive\objects', 'archive\manifests')) {
        $path = Join-Path $RuntimeRoot $dir
        if (Test-Path -LiteralPath $path -PathType Container) {
            $count = @(Get-ChildItem -LiteralPath $path -Recurse -File -Force -ErrorAction Stop).Count
            if ($count -gt 0) { $problems.Add("$dir 下有 $count 个已归档文件") }
        }
    }
    $sessionFiles = @(Get-ChildItem -LiteralPath $RuntimeRoot -Recurse -File -Force -Filter '*.jsonl' -ErrorAction SilentlyContinue)
    if ($sessionFiles.Count -gt 0) { $problems.Add("运行时根下出现 $($sessionFiles.Count) 个 JSONL 会话文件") }
    return [pscustomobject]@{
        Ok = ($problems.Count -eq 0)
        Problems = $problems.ToArray()
        Records = $records
    }
}

# Running agent-memory processes started before the installed binary was last
# replaced. Windows keeps the old image mapped for a process that was running
# when the file was renamed and replaced, so such a process (typically an MCP
# server a host started at launch) keeps executing the old build until its host
# restarts. -Processes injects fixtures for tests; by default the live process
# table is read (read-only).
function Get-AgentMemoryStaleProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$ExePath,
        [object[]]$Processes,
        # Every process running this path, regardless of start time (used when a
        # replacement failed and the old file is still in place).
        [switch]$AnyStartTime
    )
    if (-not (Test-Path -LiteralPath $ExePath -PathType Leaf)) { return @() }
    $item = Get-Item -LiteralPath $ExePath
    $installedAt = $item.LastWriteTime
    if ($item.CreationTime -gt $installedAt) { $installedAt = $item.CreationTime }
    $target = [System.IO.Path]::GetFullPath($ExePath)
    if ($null -eq $Processes) {
        $all = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue)
        $byId = @{}
        foreach ($p in $all) { $byId[[int]$p.ProcessId] = $p }
        $Processes = @($all | Where-Object { $_.Name -eq 'agent-memory.exe' } | ForEach-Object {
            $parent = $byId[[int]$_.ParentProcessId]
            [pscustomobject]@{
                ProcessId      = [int]$_.ProcessId
                ExecutablePath = $_.ExecutablePath
                StartTime      = $_.CreationDate
                CommandLine    = $_.CommandLine
                ParentName     = if ($parent) { $parent.Name } else { '' }
            }
        })
    }
    $stale = @()
    foreach ($p in $Processes) {
        if (-not $p.ExecutablePath -or -not $p.StartTime) { continue }
        $path = [System.IO.Path]::GetFullPath([string]$p.ExecutablePath)
        if (-not [string]::Equals($path, $target, [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        if ($AnyStartTime -or [datetime]$p.StartTime -lt $installedAt) { $stale += $p }
    }
    return $stale
}
