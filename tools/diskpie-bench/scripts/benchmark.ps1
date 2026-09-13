param([int]$Runs = 7, [switch]$SkipBuild, [switch]$BuildOnly, [switch]$Include100kReal)
$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../..'))
$baseline = Join-Path $repo 'target/bench-baseline'
if (!(Test-Path -LiteralPath (Join-Path $baseline 'SOURCE_COMMIT.txt'))) { throw 'Capture git archive HEAD into target/bench-baseline before editing production code.' }
$harness = Join-Path $repo 'tools/diskpie-bench'
$baselineHarness = Join-Path $baseline 'tools/diskpie-bench'
New-Item -ItemType Directory -Force -Path $baselineHarness | Out-Null
Copy-Item -LiteralPath (Join-Path $harness 'Cargo.toml') -Destination $baselineHarness
Copy-Item -LiteralPath (Join-Path $harness 'Cargo.lock') -Destination $baselineHarness
Copy-Item -LiteralPath (Join-Path $harness 'src') -Destination $baselineHarness -Recurse -Force
$prototype = 'crates/diskpie-platform/src/windows/batch_prototype.rs'
Copy-Item -LiteralPath (Join-Path $repo $prototype) -Destination (Join-Path $baseline $prototype) -Force
$builds = @{ baseline = (Join-Path $repo 'target/bench-build-baseline'); current = (Join-Path $repo 'target/bench-build-current') }
if (!$SkipBuild) {
    & cargo build --release --locked --manifest-path (Join-Path $baselineHarness 'Cargo.toml') --target-dir $builds.baseline
    if ($LASTEXITCODE -ne 0) { throw 'Baseline build failed' }
    & cargo build --release --locked --manifest-path (Join-Path $harness 'Cargo.toml') --target-dir $builds.current
    if ($LASTEXITCODE -ne 0) { throw 'Current build failed' }
}
if ($BuildOnly) { return }
$results = Join-Path $harness 'results'
New-Item -ItemType Directory -Force -Path $results | Out-Null
$realCounts = @(10000)
if ($Include100kReal) { $realCounts += 100000 }
foreach ($count in $realCounts) {
    $fixture = Join-Path $repo "target/bench-fixture-$count"
    New-Item -ItemType Directory -Force -Path $fixture | Out-Null
    for ($index = 0; $index -lt $count; $index++) {
        $file = Join-Path $fixture ('file-{0:D7}.dat' -f $index)
        if (!(Test-Path -LiteralPath $file)) { [IO.File]::WriteAllBytes($file, [byte[]](0..63)) }
    }
}
$scenarios = @()
foreach ($name in @('flat','wide','balanced','deep','omissions','slow')) { foreach ($count in @(100000,1000000)) { $scenarios += @{Name=$name;Count=$count} } }
foreach ($count in $realCounts) { $scenarios += @{Name='real';Count=$count}; $scenarios += @{Name='batch';Count=$count} }
$rows = [Collections.Generic.List[object]]::new()
for ($run = 1; $run -le $Runs; $run++) {
    $order = if ($run % 2 -eq 1) { @('baseline','current') } else { @('current','baseline') }
    foreach ($scenario in $scenarios) {
        foreach ($variant in $order) {
            $exe = Join-Path $builds[$variant] 'release/diskpie-bench.exe'
            $prefix = "$variant-$($scenario.Name)-$($scenario.Count)-$run"
            $stdout = Join-Path $results "$prefix.stdout.csv"
            $stderr = Join-Path $results "$prefix.stderr.txt"
            $arguments = @('--variant', $variant, '--run', $run, '--scenario', $scenario.Name, '--entries', $scenario.Count)
            if ($scenario.Name -in @('real','batch')) { $arguments += @('--root', ('"{0}"' -f (Join-Path $repo "target/bench-fixture-$($scenario.Count)"))) }
            $benchProcess = Start-Process -FilePath $exe -ArgumentList $arguments -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
            # Keep the process handle open so Windows PowerShell can retrieve
            # ExitCode after Refresh/HasExited polling.
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
            if ($benchProcess.ExitCode -ne 0) { throw "Sample failed: $prefix : $(Get-Content -LiteralPath $stderr -Raw)" }
            $row = Import-Csv -LiteralPath $stdout
            $row | Add-Member -NotePropertyName sampled_peak_working_bytes -NotePropertyValue $working
            $row | Add-Member -NotePropertyName sampled_peak_private_bytes -NotePropertyValue $private
            $row | Add-Member -NotePropertyName sampled_peak_handles -NotePropertyValue $handles
            $row | Add-Member -NotePropertyName sampled_cpu_ms -NotePropertyValue $cpu
            $rows.Add($row)
            $rows | Export-Csv -LiteralPath (Join-Path $results 'samples.csv') -NoTypeInformation
            Write-Host "$prefix scan=$($row.scan_reduce_ms)ms snapshot=$($row.snapshot_ms)ms layout=$($row.layout_ms)ms peak=$working"
        }
    }
}
$sources = @('crates/diskpie-scan/src/coordinator.rs','crates/diskpie-core/src/model.rs','crates/diskpie-core/src/sunburst.rs','crates/diskpie-app/src/session.rs','tools/diskpie-bench/src/main.rs','tools/diskpie-bench/Cargo.lock')
$fingerprints = @{}
foreach ($source in $sources) { $fingerprints[$source] = (Get-FileHash -LiteralPath (Join-Path $repo $source) -Algorithm SHA256).Hash }
@{ DateUTC = [DateTime]::UtcNow.ToString('o'); Baseline = (Get-Content -LiteralPath (Join-Path $baseline 'SOURCE_COMMIT.txt')); OS = [Environment]::OSVersion.VersionString; Processors = [Environment]::ProcessorCount; CPU = (Get-CimInstance Win32_Processor | Select-Object -ExpandProperty Name); Rust = (& rustc --version); Runs = $Runs; SamplingMilliseconds = 10; SourceSHA256 = $fingerprints; CachePolicy = 'warm filesystem cache, alternating A/B order, fresh process per sample' } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $results 'environment.json')
foreach ($count in $realCounts) {
    & (Join-Path $builds.current 'release/diskpie-bench.exe') --scenario differential --root (Join-Path $repo "target/bench-fixture-$count") | Set-Content -LiteralPath (Join-Path $results "differential-$count.txt")
    if ($LASTEXITCODE -ne 0) { throw 'Batch differential comparison failed' }
}
