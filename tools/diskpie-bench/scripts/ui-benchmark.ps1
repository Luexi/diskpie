param([int]$Runs = 7, [switch]$SkipBuild, [switch]$BuildOnly)
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$baseline = Join-Path $repo 'target/bench-baseline'
$harness = Join-Path $repo 'tools/diskpie-bench'
if (!(Test-Path -LiteralPath (Join-Path $baseline 'SOURCE_COMMIT.txt'))) { throw 'Missing archived baseline' }
$baselineHarness = Join-Path $baseline 'tools/diskpie-bench'
New-Item -ItemType Directory -Force -Path $baselineHarness | Out-Null
Copy-Item -LiteralPath (Join-Path $harness 'ui_frames.inc.rs') -Destination $baselineHarness -Force
$baselineShell = Join-Path $baseline 'crates/diskpie/src/shell.rs'
$source = [IO.File]::ReadAllText($baselineShell)
$include = '    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/diskpie-bench/ui_frames.inc.rs"));'
if (!$source.Contains('ui_frames.inc.rs')) {
    $closingBrace = $source.LastIndexOf('}')
    if ($closingBrace -lt 0 -or $source.Substring($closingBrace).Trim() -ne '}') { throw 'Unexpected baseline test module ending' }
    [IO.File]::WriteAllText($baselineShell, $source.Substring(0, $closingBrace) + $include + "`n}`n", [Text.UTF8Encoding]::new($false))
}
$builds = @{ baseline = (Join-Path $repo 'target/bench-ui-baseline'); current = (Join-Path $repo 'target/bench-ui-current') }
$roots = @{ baseline = $baseline; current = $repo }
$executables = @{}
foreach ($variant in @('baseline','current')) {
    New-Item -ItemType Directory -Force -Path $builds[$variant] | Out-Null
    $artifacts = Join-Path $builds[$variant] 'artifacts.jsonl'
    if (!$SkipBuild) {
        & cargo test --release --locked --no-run --message-format=json -p diskpie --manifest-path (Join-Path $roots[$variant] 'Cargo.toml') --target-dir $builds[$variant] | Set-Content -LiteralPath $artifacts
        if ($LASTEXITCODE -ne 0) { throw "UI benchmark build failed: $variant" }
    }
    $artifact = Get-Content -LiteralPath $artifacts | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'diskpie' -and $_.profile.test -and $_.executable } | Select-Object -Last 1
    if (!$artifact) { throw "No executable in $artifacts" }
    $executables[$variant] = $artifact.executable
}
if ($BuildOnly) { return }
$results = Join-Path $harness 'results/ui'
New-Item -ItemType Directory -Force -Path $results | Out-Null
$processRows = [Collections.Generic.List[object]]::new()
$environmentNames = @('DISKPIE_UI_BENCH_ENTRIES','DISKPIE_UI_BENCH_VARIANT','DISKPIE_UI_BENCH_RUN','DISKPIE_UI_BENCH_OUTPUT')
$savedEnvironment = @{}
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    for ($run = 1; $run -le $Runs; $run++) {
        $order = if ($run % 2 -eq 1) { @('baseline','current') } else { @('current','baseline') }
        foreach ($count in @(100000,1000000)) {
            foreach ($variant in $order) {
                $prefix = "$variant-$count-$run"
                $output = Join-Path $results "$prefix.frames.csv"
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_ENTRIES', "$count", 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_VARIANT', $variant, 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_RUN', "$run", 'Process')
                [Environment]::SetEnvironmentVariable('DISKPIE_UI_BENCH_OUTPUT', $output, 'Process')
                $stdout = Join-Path $results "$prefix.stdout.txt"
                $stderr = Join-Path $results "$prefix.stderr.txt"
                $benchProcess = Start-Process -FilePath $executables[$variant] -ArgumentList @('--exact','shell::tests::benchmark::frames_csv','--ignored','--nocapture','--test-threads=1') -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
                $null = $benchProcess.Handle
                $working = 0L; $private = 0L; $handles = 0; $cpu = 0.0
                while (!$benchProcess.HasExited) {
                    try {
                        $benchProcess.Refresh()
                        $working = [Math]::Max($working, $benchProcess.WorkingSet64)
                        $private = [Math]::Max($private, $benchProcess.PrivateMemorySize64)
                        $handles = [Math]::Max($handles, $benchProcess.HandleCount)
                        $cpu = [Math]::Max($cpu, $benchProcess.TotalProcessorTime.TotalMilliseconds)
                    } catch [System.InvalidOperationException] { }
                    Start-Sleep -Milliseconds 10
                }
                $benchProcess.WaitForExit()
                if ($benchProcess.ExitCode -ne 0) { throw "UI sample failed: $prefix : $(Get-Content -LiteralPath $stderr -Raw)" }
                if (!(Test-Path -LiteralPath $output)) { throw "UI test did not write $output" }
                $processRows.Add([pscustomobject]@{ variant=$variant;run=$run;entries=$count;sampled_peak_working_bytes=$working;sampled_peak_private_bytes=$private;sampled_peak_handles=$handles;sampled_cpu_ms=$cpu })
                $processRows | Export-Csv -LiteralPath (Join-Path $results 'processes.csv') -NoTypeInformation
                Write-Host "UI $prefix CPU=${cpu}ms peak=$working"
            }
        }
    }
} finally {
    foreach ($name in $environmentNames) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
}
$fingerprints = @{}
foreach ($source in @('crates/diskpie/src/shell.rs','tools/diskpie-bench/ui_frames.inc.rs','crates/diskpie-app/src/item_list_service.rs')) { $fingerprints[$source] = (Get-FileHash -LiteralPath (Join-Path $repo $source) -Algorithm SHA256).Hash }
@{ DateUTC=[DateTime]::UtcNow.ToString('o');Baseline=(Get-Content -LiteralPath (Join-Path $baseline 'SOURCE_COMMIT.txt'));Runs=$Runs;Viewport='1280x800 logical pixels';FramesPerStage=200;SourceSHA256=$fingerprints;Method='egui run_ui CPU geometry; no HWND, GPU tessellation/rasterization or display/vsync' } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $results 'environment.json')
