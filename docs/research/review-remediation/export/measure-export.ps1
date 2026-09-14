$ErrorActionPreference = 'Stop'
$evidenceDirectory = $PSScriptRoot
$csvPath = Join-Path $evidenceDirectory 'export-seven-runs.csv'
if (Test-Path -LiteralPath $csvPath) { throw 'Refusing to overwrite existing measurements' }
$results = [Collections.Generic.List[object]]::new()
foreach ($fixtureName in @('event', 'blank')) {
    $fixturePath = Join-Path $evidenceDirectory "fixture-$fixtureName"
    for ($run = 1; $run -le 7; $run++) {
        $variants = if ($run % 2 -eq 1) { @('before', 'after') } else { @('after', 'before') }
        foreach ($variant in $variants) {
            $executable = Join-Path $evidenceDirectory "export-native-$variant.exe"
            $line = (& $executable measure $fixturePath | Out-String).Trim()
            if ($LASTEXITCODE -ne 0) { throw "$variant/$fixtureName/$run failed: $line" }
            if ($line -notmatch '^schema=(\d+) sources=(\d+) input_bytes=(\d+) lines=(\d+) artifact_bytes=(\d+) elapsed_ms=([\d.]+)$') {
                throw "Unexpected helper output: $line"
            }
            $expectedSchema = if ($variant -eq 'before') { 1 } else { 2 }
            $expectedBytes = if ($fixtureName -eq 'event') { 7930000 } else { 1048576 }
            $expectedLines = if ($fixtureName -eq 'event') { 65000 } else { 1048576 }
            if ([int]$Matches[1] -ne $expectedSchema -or [int]$Matches[2] -ne 8 -or
                [int]$Matches[3] -ne $expectedBytes -or [int]$Matches[4] -ne $expectedLines -or
                [int]$Matches[5] -gt 8388608) { throw "Unexpected measured input/output contract: $line" }
            $results.Add([pscustomobject]@{
                Fixture = $fixtureName; Run = $run; Variant = $variant;
                Schema = [int]$Matches[1]; Sources = [int]$Matches[2]; InputBytes = [int]$Matches[3];
                Lines = [int]$Matches[4]; ArtifactBytes = [int]$Matches[5];
                ElapsedMs = [double]::Parse($Matches[6], [Globalization.CultureInfo]::InvariantCulture)
            })
            "fixture=$fixtureName run=$run variant=$variant $line"
        }
    }
}
$results | Export-Csv -LiteralPath $csvPath -NoTypeInformation
foreach ($fixtureName in @('event', 'blank')) {
    $before = @($results | Where-Object { $_.Fixture -eq $fixtureName -and $_.Variant -eq 'before' } | Sort-Object ElapsedMs)[3].ElapsedMs
    $after = @($results | Where-Object { $_.Fixture -eq $fixtureName -and $_.Variant -eq 'after' } | Sort-Object ElapsedMs)[3].ElapsedMs
    "median fixture=$fixtureName before_ms=$before after_ms=$after change_percent=$([math]::Round(($after / $before - 1) * 100, 2))"
}
