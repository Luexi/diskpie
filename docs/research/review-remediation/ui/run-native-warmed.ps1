$ErrorActionPreference = 'Stop'
$repo = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))
$out = Join-Path $PSScriptRoot 'native-warmed'
if (Test-Path -LiteralPath $out) { throw 'Never overwrite a previous native series.' }
New-Item -ItemType Directory -Path $out | Out-Null
$historical = Get-Content -LiteralPath (Join-Path $repo 'tools/diskpie-bench/results/design/build-manifest.json') -Raw | ConvertFrom-Json
$before = $historical.Variants.after.LayoutExecutable
if ((Get-FileHash -LiteralPath $before.Path -Algorithm SHA256).Hash -ne $before.SHA256) { throw 'Baseline changed.' }
$binaries = @{
    before=$before.Path
    after=(Join-Path $PSScriptRoot 'build/release/diskpie-bench.exe')
}
$identities = foreach ($variant in @('before','after')) {
    [pscustomobject]@{Variant=$variant;Path=$binaries[$variant];SHA256=(Get-FileHash -LiteralPath $binaries[$variant] -Algorithm SHA256).Hash}
}
$identities | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'binaries.json') -Encoding utf8
$rows = [Collections.Generic.List[object]]::new()
function Invoke-Native([string]$exe, [string]$variant, [int]$run, [string]$name) {
    $sampleArguments = @('--variant',$variant,'--run',"$run",'--scenario','real','--entries','10000','--root',(Join-Path $repo 'target/bench-fixture-10000'))
    $process = Start-Process -FilePath $exe -ArgumentList $sampleArguments -WindowStyle Hidden -PassThru -Wait -RedirectStandardOutput (Join-Path $out "$name.stdout.csv") -RedirectStandardError (Join-Path $out "$name.stderr.txt")
    if ($process.ExitCode -ne 0) { throw "Failed: $name" }
}
for ($run=1; $run -le 7; $run++) {
    $order=if($run%2 -eq 1){@('before','after')}else{@('after','before')}
    foreach($variant in $order) {
        # Identical baseline warmup before EACH measured process, not just the
        # first variant. Its complete scan/materialization/layout/cancel cycle
        # is outside all timings reported by the following fresh process.
        Invoke-Native $binaries.before 'warmup' $run "warmup-$variant-$run"
        Invoke-Native $binaries[$variant] $variant $run "$variant-$run"
        $rows.Add((Import-Csv -LiteralPath (Join-Path $out "$variant-$run.stdout.csv")))
    }
    Write-Output "Warm-cache native pair $run/7"
}
foreach($identity in $identities) {
    if ((Get-FileHash -LiteralPath $identity.Path -Algorithm SHA256).Hash -ne $identity.SHA256) { throw 'Measured binary changed.' }
}
$rows | Export-Csv -LiteralPath (Join-Path $out 'measurements.csv') -NoTypeInformation
[pscustomobject]@{DateUTC=[DateTime]::UtcNow.ToString('o');Runs=7;FixtureAccess='read-only';Warmup='One entire baseline process using the same real fixture before every measured process; warmup excluded from reported timers';ProcessSampling='none in this short control'} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $out 'method.json') -Encoding utf8
