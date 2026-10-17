param(
    [ValidateSet("lint", "check", "check-full")]
    [string]$Mode = "check"
)

$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code $LASTEXITCODE`: $Command $($Arguments -join ' ')"
    }
}

Invoke-Checked cargo @("fmt", "--check")
Invoke-Checked cargo @(
    "clippy",
    "--all-targets",
    "--locked",
    "--",
    "-D",
    "warnings"
)

if ($Mode -eq "lint") {
    return
}

if ($Mode -eq "check-full") {
    Invoke-Checked just @("test-full")
} else {
    Invoke-Checked just @("test")
}
Invoke-Checked cargo @("build", "--locked")
