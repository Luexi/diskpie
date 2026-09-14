$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$out = Join-Path $PSScriptRoot 'results'
if (Test-Path -LiteralPath $out) { throw 'Never overwrite an earlier comparison.' }
New-Item -ItemType Directory -Path $out | Out-Null
$historical = Get-Content -LiteralPath (Join-Path $repo 'tools/diskpie-bench/results/design/build-manifest.json') -Raw | ConvertFrom-Json
$baseline = $historical.Variants.after.LayoutExecutable
if ((Get-FileHash -LiteralPath $baseline.Path -Algorithm SHA256).Hash -ne $baseline.SHA256) { throw 'Post-redesign baseline binary changed.' }
$binaries = @{
    before = $baseline.Path
    after = Join-Path $PSScriptRoot 'build/release/diskpie-bench.exe'
}
$micros = @{
    before = Join-Path $repo 'target/review-ui-perf-20260907-215950-locked/build/release/diskpie-review-ui-perf.exe'
    after = Join-Path $PSScriptRoot 'build/release/diskpie-review-ui-perf.exe'
}
$reviewHashes = Get-Content -LiteralPath (Join-Path $repo 'target/review-ui-perf-20260907-215950-locked/hashes.json') -Raw | ConvertFrom-Json
$microBaseline = $reviewHashes | Where-Object { $_.Path -eq $micros.before }
if (!$microBaseline -or (Get-FileHash -LiteralPath $micros.before -Algorithm SHA256).Hash -ne $microBaseline.Hash) { throw 'Review microbenchmark baseline binary changed.' }
$identities = foreach ($kind in @('harness', 'micro')) {
    $set = if ($kind -eq 'harness') { $binaries } else { $micros }
    foreach ($variant in @('before', 'after')) {
        [pscustomobject]@{ Kind=$kind; Variant=$variant; Path=$set[$variant]; SHA256=(Get-FileHash -LiteralPath $set[$variant] -Algorithm SHA256).Hash }
    }
}
$identities | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'binaries.json') -Encoding utf8
$processes = [Collections.Generic.List[object]]::new()
$layouts = [Collections.Generic.List[object]]::new()
function Invoke-Sample([string]$exe, [string[]]$arguments, [string]$name) {
    $stdout = Join-Path $out "$name.stdout.csv"
    $stderr = Join-Path $out "$name.stderr.txt"
    $launch = @{ FilePath=$exe; WindowStyle='Hidden'; PassThru=$true; RedirectStandardOutput=$stdout; RedirectStandardError=$stderr }
    if ($arguments.Count) { $launch.ArgumentList=$arguments }
    $sample = Start-Process @launch
    $null = $sample.Handle
    $working = 0L; $private = 0L; $handles = 0; $cpu = 0.0; $observations = 0
    while (!$sample.HasExited) {
        try {
            $sample.Refresh()
            $working = [Math]::Max($working, $sample.WorkingSet64)
            $private = [Math]::Max($private, $sample.PrivateMemorySize64)
            $handles = [Math]::Max($handles, $sample.HandleCount)
            $cpu = [Math]::Max($cpu, $sample.TotalProcessorTime.TotalMilliseconds)
            $observations++
        } catch [System.InvalidOperationException] { }
        Start-Sleep -Milliseconds 10
    }
    $sample.WaitForExit()
    if ($sample.ExitCode -ne 0) { throw "Sample failed: $name" }
    $processes.Add([pscustomobject]@{sample=$name; sampled_peak_working_bytes=$working;sampled_peak_private_bytes=$private;sampled_peak_handles=$handles;sampled_cpu_ms=$cpu;observations=$observations})
}
for ($run=1; $run -le 7; $run++) {
    $order = if ($run % 2 -eq 1) { @('before', 'after') } else { @('after', 'before') }
    foreach ($variant in $order) {
        Invoke-Sample $micros[$variant] @() "micro-$variant-$run"
    }
    foreach ($scenario in @('balanced', 'real')) {
        foreach ($variant in $order) {
            $count = if ($scenario -eq 'real') { '10000' } else { '1000000' }
            $sampleArguments = @('--variant', $variant, '--run', "$run", '--scenario', $scenario, '--entries', $count)
            if ($scenario -eq 'real') { $sampleArguments += @('--root', (Join-Path $repo 'target/bench-fixture-10000')) }
            $name = "$scenario-$variant-$run"
            Invoke-Sample $binaries[$variant] $sampleArguments $name
            $layouts.Add((Import-Csv -LiteralPath (Join-Path $out "$name.stdout.csv")))
        }
    }
    Write-Output "Completed alternating run $run/7"
}
foreach ($identity in $identities) {
    if ((Get-FileHash -LiteralPath $identity.Path -Algorithm SHA256).Hash -ne $identity.SHA256) { throw 'A measured binary changed.' }
}
$processes | Export-Csv -LiteralPath (Join-Path $out 'processes.csv') -NoTypeInformation
$layouts | Export-Csv -LiteralPath (Join-Path $out 'layouts.csv') -NoTypeInformation
[pscustomobject]@{DateUTC=[DateTime]::UtcNow.ToString('o');Runs=7;Fixture=(Join-Path $repo 'target/bench-fixture-10000');FixtureAccess='read-only';SamplingMilliseconds=10;Note='CPU and process samples; not GPU/display FPS or causal allocation accounting.'} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'method.json') -Encoding utf8
