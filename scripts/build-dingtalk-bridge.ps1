param([string]$NodeExecutable = 'node.exe', [string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$bridgeRoot = Join-Path $projectRoot 'bridge/dingtalk'
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $projectRoot 'prebuilt/dingtalk' }
$OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
$node = (Get-Command $NodeExecutable -ErrorAction Stop).Source
$nodeVersion = (& $node --version).Trim()
if ($LASTEXITCODE -ne 0 -or $nodeVersion -notmatch '^v(2[4-9]|[3-9][0-9])\.\d+\.\d+$') { throw 'Node.js 24 or later is required to build the bridge' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
Push-Location $bridgeRoot
try {
    npm.cmd ci --ignore-scripts --no-audit --no-fund
    if ($LASTEXITCODE -ne 0) { throw 'Bridge dependencies failed' }
    & $node --test bridge.test.mjs
    if ($LASTEXITCODE -ne 0) { throw 'Bridge tests failed' }
    & $node node_modules/esbuild/bin/esbuild main.mjs --bundle --platform=node --format=cjs --legal-comments=external "--outfile=$OutputDirectory/bridge.cjs"
    if ($LASTEXITCODE -ne 0) { throw 'Bridge bundling failed' }
    Copy-Item -LiteralPath $node -Destination (Join-Path $OutputDirectory 'node.exe')
    Copy-Item -LiteralPath (Join-Path $bridgeRoot 'UPSTREAM-LICENSE.txt') -Destination $OutputDirectory
    Invoke-WebRequest -Uri "https://raw.githubusercontent.com/nodejs/node/$nodeVersion/LICENSE" -OutFile (Join-Path $OutputDirectory 'NODE-LICENSE.txt')
    $notices = [Text.StringBuilder]::new()
    Get-ChildItem node_modules -Filter package.json -File -Recurse | Sort-Object FullName | ForEach-Object {
        $package = Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json
        Get-ChildItem -LiteralPath $_.DirectoryName -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)(\..*)?$' } | ForEach-Object {
            [void]$notices.AppendLine("===== $($package.name) $($package.version) / $($_.Name) =====")
            [void]$notices.AppendLine([IO.File]::ReadAllText($_.FullName))
        }
    }
    [IO.File]::WriteAllText((Join-Path $OutputDirectory 'DEPENDENCY-LICENSES.txt'),$notices.ToString(),[Text.UTF8Encoding]::new($false))
    $hashes = @{}
    @('node.exe','bridge.cjs') | ForEach-Object { $hashes[$_] = (Get-FileHash -LiteralPath (Join-Path $OutputDirectory $_) -Algorithm SHA256).Hash.ToLowerInvariant() }
    @{
        upstream='https://github.com/soimy/openclaw-channel-dingtalk'
        revision='63956bc8ed9c168045a671271abb2ee1530c123d'
        adaptation='Standalone Stream and Markdown transport; no OpenClaw runtime'
        node=$nodeVersion
        sha256=$hashes
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'build-metadata.json') -Encoding utf8
} finally { Pop-Location }
