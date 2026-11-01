param(
    [ValidateSet('once', 'daemon', 'collect', 'digest', 'test')]
    [string]$Mode = 'once',
    [switch]$Send
)

$ErrorActionPreference = 'Stop'
$cargoCommand = Get-Command cargo -ErrorAction SilentlyContinue
$cargoPath = if ($cargoCommand) { $cargoCommand.Source } else {
    Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
}
if (-not (Test-Path -LiteralPath $cargoPath)) {
    throw 'Rust/Cargo is not installed. Install from https://rust-lang.org/tools/install'
}
if ($Send -and $Mode -in @('collect', 'test')) {
    throw '-Send is supported only for once, daemon and digest.'
}

Push-Location $PSScriptRoot
try {
    if ($Mode -eq 'test') {
        & $cargoPath fmt --all -- --check
        if ($LASTEXITCODE -ne 0) { throw 'Formatting check failed.' }
        & $cargoPath clippy --locked --all-targets -- -D warnings
        if ($LASTEXITCODE -ne 0) { throw 'Clippy failed.' }
        & $cargoPath test --locked --all-targets
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
    } else {
        $runArguments = @('run', '--locked', '--')
        switch ($Mode) {
            'once' { $runArguments += @('run', '--once') }
            'daemon' { $runArguments += 'run' }
            'collect' { $runArguments += 'collect' }
            'digest' { $runArguments += 'digest' }
        }
        if ($Send) { $runArguments += '--send' }
        & $cargoPath @runArguments
        if ($LASTEXITCODE -ne 0) { throw "Command failed with exit code $LASTEXITCODE." }
    }
} finally {
    Pop-Location
}
