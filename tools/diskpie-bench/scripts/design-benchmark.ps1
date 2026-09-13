param([int]$Runs = 7, [switch]$SkipBuild, [switch]$BuildOnly)
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$before = Join-Path $repo 'target/design-before'
if (!(Test-Path -LiteralPath (Join-Path $before 'Cargo.toml'))) { throw 'Archive the current working sources into target/design-before before editing.' }
$out = Join-Path $repo 'tools/diskpie-bench/results/design'
New-Item -ItemType Directory -Force -Path $out | Out-Null
$roots = @{ before = $before; after = $repo }
$uiBuilds = @{ before = (Join-Path $repo 'target/design-ui-before-build'); after = (Join-Path $repo 'target/bench-ui-current') }
$layoutBuilds = @{ before = (Join-Path $repo 'target/bench-build-baseline'); after = (Join-Path $repo 'target/bench-build-current') }
$manifestPath = Join-Path $repo 'target/design-build-manifest.json'

function Get-SourceHashes([string]$sourceRoot) {
    # Walk source directories without descending into Cargo artifacts or Git.
    # Include embedded assets as well as Rust, translations and build inputs.
    $sourceRoot = [IO.Path]::GetFullPath($sourceRoot)
    $sourcePrefix = $sourceRoot.TrimEnd('\') + '\'
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($sourceRoot)
    $files = [Collections.Generic.List[string]]::new()
    while ($pending.Count -ne 0) {
        foreach ($entry in Get-ChildItem -LiteralPath $pending.Pop() -Force) {
            if ($entry.PSIsContainer) {
                if ($entry.Name -notin @('target', '.git')) { $pending.Push($entry.FullName) }
            } else {
                $relative = $entry.FullName.Substring($sourcePrefix.Length).Replace('\', '/')
                if ($entry.Extension -in @('.rs', '.ftl') -or
                    $entry.Name -in @('Cargo.toml', 'Cargo.lock', 'rust-toolchain', 'rust-toolchain.toml') -or
                    ($entry.Directory.Name -eq '.cargo' -and $entry.Name -in @('config', 'config.toml')) -or
                    ($relative.StartsWith('assets/') -and $entry.Extension -in @('.ttf', '.otf', '.ttc', '.png', '.ico'))) {
                    $files.Add($relative)
                }
            }
        }
    }
    $hashes = [ordered]@{}
    foreach ($path in $files | Sort-Object) {
        $hashes[$path] = (Get-FileHash -LiteralPath (Join-Path $sourceRoot $path) -Algorithm SHA256).Hash
    }
    return [pscustomobject]$hashes
}

function Get-Toolchain {
    $lines = & rustc -Vv
    if ($LASTEXITCODE -ne 0) { throw 'Cannot identify the Rust toolchain.' }
    return [string]::Join([Environment]::NewLine, $lines)
}

function Get-BinaryIdentity([string]$path) {
    $path = [IO.Path]::GetFullPath($path)
    return [pscustomobject]@{ Path=$path; SHA256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
}

function Assert-HashesEqual($expected, $actual, [string]$label) {
    if (($expected | ConvertTo-Json -Depth 10 -Compress) -cne ($actual | ConvertTo-Json -Depth 10 -Compress)) {
        throw "$label changed. Rebuild with BuildOnly after sources are stable; archive partial results before measuring again."
    }
}

function Assert-ManifestCurrent($manifest) {
    if ($manifest.Schema -ne 1) { throw 'Unsupported or missing design build manifest; run BuildOnly.' }
    if ($manifest.Toolchain -cne (Get-Toolchain)) { throw 'Toolchain differs from the compiled manifest; run BuildOnly.' }
    if (!$manifest.Variants.before.SourceSHA256.'tools/diskpie-bench/ui_frames.inc.rs' -or
        $manifest.Variants.before.SourceSHA256.'tools/diskpie-bench/ui_frames.inc.rs' -cne
        $manifest.Variants.after.SourceSHA256.'tools/diskpie-bench/ui_frames.inc.rs') {
        throw 'Before/after UI samples must use the same shared benchmark body.'
    }
    foreach ($variant in @('before','after')) {
        $build = $manifest.Variants.$variant
        if ($build.Root -ne [IO.Path]::GetFullPath($roots[$variant])) { throw "Source root differs: $variant" }
        $uiDirectory = [IO.Path]::GetFullPath($uiBuilds[$variant]).TrimEnd('\') + '\'
        if (!$build.UiExecutable.Path.StartsWith($uiDirectory, [StringComparison]::OrdinalIgnoreCase) -or
            $build.LayoutExecutable.Path -ne [IO.Path]::GetFullPath((Join-Path $layoutBuilds[$variant] 'release/diskpie-bench.exe'))) {
            throw "Executable paths differ from the isolated build directories: $variant"
        }
        Assert-HashesEqual $build.SourceSHA256 (Get-SourceHashes $roots[$variant]) "Sources ($variant)"
        Assert-HashesEqual $build.UiExecutable (Get-BinaryIdentity $build.UiExecutable.Path) "UI executable ($variant)"
        Assert-HashesEqual $build.LayoutExecutable (Get-BinaryIdentity $build.LayoutExecutable.Path) "Layout executable ($variant)"
    }
}

function Assert-EmptyResults {
    $existing = Get-ChildItem -LiteralPath $out -File | Where-Object {
        $_.Extension -eq '.csv' -or $_.Name -in @('environment.json', 'build-manifest.json') -or
        $_.Name -like '*.stdout.txt' -or $_.Name -like '*.stderr.txt'
    } | Select-Object -First 1
    if ($existing) {
        throw "Results already contain '$($existing.Name)'. Archive or explicitly remove the old run before measuring; partial results are never overwritten."
    }
}

if (!$BuildOnly) { Assert-EmptyResults }
$uiExecutables = @{}
if (!$SkipBuild) {
    $buildStarted = [DateTime]::UtcNow.ToString('o')
    $toolchain = Get-Toolchain
    $sourceHashes = @{}
    foreach ($variant in @('before','after')) { $sourceHashes[$variant] = Get-SourceHashes $roots[$variant] }
    $variants = [ordered]@{}
    foreach ($variant in @('before','after')) {
        $artifacts = Join-Path $uiBuilds[$variant] 'design-artifacts.jsonl'
        New-Item -ItemType Directory -Force -Path $uiBuilds[$variant] | Out-Null
        & cargo test --release --locked --no-run --message-format=json -p diskpie --manifest-path (Join-Path $roots[$variant] 'Cargo.toml') --target-dir $uiBuilds[$variant] | Set-Content -LiteralPath $artifacts
        if ($LASTEXITCODE -ne 0) { throw "UI build failed: $variant" }
        & cargo build --release --locked --manifest-path (Join-Path $roots[$variant] 'tools/diskpie-bench/Cargo.toml') --target-dir $layoutBuilds[$variant]
        if ($LASTEXITCODE -ne 0) { throw "Layout build failed: $variant" }
        $artifact = Get-Content -LiteralPath $artifacts | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'diskpie' -and $_.profile.test -and $_.executable } | Select-Object -Last 1
        if (!$artifact) { throw "No test executable for $variant" }
        $variants[$variant] = [pscustomobject]@{
            Root=[IO.Path]::GetFullPath($roots[$variant]); SourceSHA256=$sourceHashes[$variant]
            UiExecutable=(Get-BinaryIdentity $artifact.executable)
            LayoutExecutable=(Get-BinaryIdentity (Join-Path $layoutBuilds[$variant] 'release/diskpie-bench.exe'))
        }
    }
    $manifest = [pscustomobject]@{
        Schema=1; BuildStartedUTC=$buildStarted; BuildCompletedUTC=[DateTime]::UtcNow.ToString('o')
        Toolchain=$toolchain; Profile='release'; Locked=$true; Variants=[pscustomobject]$variants
    }
    # A successful Cargo invocation alone cannot identify mutable working sources.
    # Do not publish a manifest if anything changed during the two builds.
    Assert-ManifestCurrent $manifest
    $manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $manifestPath -Encoding utf8
} else {
    if (!(Test-Path -LiteralPath $manifestPath)) { throw 'Missing compiled design manifest; run BuildOnly before SkipBuild.' }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    Assert-ManifestCurrent $manifest
}
foreach ($variant in @('before','after')) { $uiExecutables[$variant] = $manifest.Variants.$variant.UiExecutable.Path }
if ($BuildOnly) { return }
Assert-EmptyResults
Assert-ManifestCurrent $manifest
$measurementStarted = [DateTime]::UtcNow.ToString('o')
$manifest | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $out 'build-manifest.json') -Encoding utf8
$envNames = @('DISKPIE_UI_BENCH_ENTRIES','DISKPIE_UI_BENCH_VARIANT','DISKPIE_UI_BENCH_RUN','DISKPIE_UI_BENCH_OUTPUT','DISKPIE_UI_BENCH_LIST_VISIBLE')
$saved = @{}
foreach ($name in $envNames) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$processRows = [Collections.Generic.List[object]]::new()
$layoutRows = [Collections.Generic.List[object]]::new()
function Invoke-Sample([string]$exe, [string[]]$arguments, [string]$prefix) {
    $stdout = Join-Path $out "$prefix.stdout.txt"
    $stderr = Join-Path $out "$prefix.stderr.txt"
    $sampleProcess = Start-Process -FilePath $exe -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $null = $sampleProcess.Handle
    $working = 0L; $private = 0L; $handles = 0; $cpu = 0.0
    while (!$sampleProcess.HasExited) {
        try {
            $sampleProcess.Refresh()
            $working = [Math]::Max($working, $sampleProcess.WorkingSet64)
            $private = [Math]::Max($private, $sampleProcess.PrivateMemorySize64)
            $handles = [Math]::Max($handles, $sampleProcess.HandleCount)
            $cpu = [Math]::Max($cpu, $sampleProcess.TotalProcessorTime.TotalMilliseconds)
        } catch [System.InvalidOperationException] { }
        Start-Sleep -Milliseconds 10
    }
    $sampleProcess.WaitForExit()
    if ($sampleProcess.ExitCode -ne 0) { throw "Sample failed: $prefix : $(Get-Content -LiteralPath $stderr -Raw)" }
    return [pscustomobject]@{ sample=$prefix; sampled_cpu_ms=$cpu; sampled_peak_working_bytes=$working; sampled_peak_private_bytes=$private; sampled_peak_handles=$handles }
}
try {
    for ($run = 1; $run -le $Runs; $run++) {
        $variants = @('before','after-map','after-list')
        $shift = ($run - 1) % 3
        $order = @($variants[$shift], $variants[($shift+1)%3], $variants[($shift+2)%3])
        foreach ($count in @(100000,1000000)) {
            foreach ($variant in $order) {
                $prefix = "$variant-$count-$run"
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_ENTRIES', "$count", 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_VARIANT', $variant, 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_RUN', "$run", 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_OUTPUT', (Join-Path $out "$prefix.frames.csv"), 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_LIST_VISIBLE', $(if ($variant -eq 'after-list') { '1' } else { '0' }), 'Process')
                $binary = if ($variant -eq 'before') { 'before' } else { 'after' }
                $processRows.Add((Invoke-Sample $uiExecutables[$binary] @('--exact','shell::tests::benchmark::frames_csv','--ignored','--nocapture','--test-threads=1') $prefix))
                $processRows | Export-Csv -LiteralPath (Join-Path $out 'processes.csv') -NoTypeInformation
                Write-Host "Frames: $prefix"
            }
        }
        foreach ($scenario in @('flat','balanced','deep')) {
            foreach ($count in @(100000,1000000)) {
                $layoutOrder = if ($run % 2 -eq 1) { @('before','after') } else { @('after','before') }
                foreach ($variant in $layoutOrder) {
                    $prefix = "layout-$variant-$scenario-$count-$run"
                    $processRows.Add((Invoke-Sample (Join-Path $layoutBuilds[$variant] 'release/diskpie-bench.exe') @('--variant',$variant,'--run',"$run",'--scenario',$scenario,'--entries',"$count") $prefix))
                    $layoutRows.Add((Import-Csv -LiteralPath (Join-Path $out "$prefix.stdout.txt")))
                    $layoutRows | Export-Csv -LiteralPath (Join-Path $out 'layouts.csv') -NoTypeInformation
                    $processRows | Export-Csv -LiteralPath (Join-Path $out 'processes.csv') -NoTypeInformation
                }
            }
        }
        Write-Host "Interleaved run $run/$Runs complete"
    }
} finally {
    foreach ($name in $envNames) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
Assert-ManifestCurrent $manifest
$hashes = [ordered]@{ before=$manifest.Variants.before.SourceSHA256; after=$manifest.Variants.after.SourceSHA256 }
$computer = Get-CimInstance Win32_ComputerSystem
$processor = Get-CimInstance Win32_Processor | Select-Object -First 1
@{ DateUTC=[DateTime]::UtcNow.ToString('o');MeasurementStartedUTC=$measurementStarted;Runs=$Runs;CPU=$processor.Name;LogicalProcessors=$computer.NumberOfLogicalProcessors;MemoryBytes=$computer.TotalPhysicalMemory;Toolchain=$manifest.Toolchain;Viewport='1280x800 logical points';SourceSHA256=$hashes;CompiledBuild=$manifest;SourcesAndBinariesVerifiedBeforeAndAfter=$true;Method='CPU-side egui run_ui; no native window/GPU/vsync. Same working-tree baseline before redesign; after-map is default and after-list exposes the optional list.' } | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath (Join-Path $out 'environment.json') -Encoding utf8
