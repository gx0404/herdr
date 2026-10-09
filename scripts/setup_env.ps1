$ErrorActionPreference = 'Stop'
$env:PSModuleAnalysisCachePath = 'NUL'
$forwarded = @($args)
$root = [IO.Directory]::GetParent($PSScriptRoot).FullName

function Test-NativePython([string]$Candidate) {
    if (-not $Candidate -or $Candidate -match '(?i)[\\/]WindowsApps[\\/]' -or
        [IO.Path]::GetExtension($Candidate) -ne '.exe' -or -not [IO.File]::Exists($Candidate)) {
        return $false
    }
    try {
        & $Candidate -I -B -c 'import sys; sys.exit(sys.version_info < (3, 11))' *> $null
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function ConvertTo-NativeArgument([string]$Value) {
    $escaped = [regex]::Replace($Value, '(\\*)"', '$1$1\"')
    return '"' + [regex]::Replace($escaped, '(\\+)$', '$1$1') + '"'
}

try {
    $python = $null
    if ($null -ne $env:HERDR_SETUP_PYTHON) {
        if (-not (Test-NativePython $env:HERDR_SETUP_PYTHON)) {
            throw 'HERDR_SETUP_PYTHON must name an existing native Python >=3.11, not a WindowsApps alias.'
        }
        $python = $env:HERDR_SETUP_PYTHON
    } else {
        $candidates = [Collections.Generic.List[string]]::new()
        foreach ($name in @('python3.exe', 'python.exe')) {
            foreach ($directory in ($env:PATH -split ';')) {
                if ($directory) { $candidates.Add([IO.Path]::Combine($directory.Trim('"'), $name)) }
            }
        }
        $localRoots = @($env:LOCALAPPDATA)
        if ($env:USERPROFILE) { $localRoots += [IO.Path]::Combine($env:USERPROFILE, 'AppData\Local') }
        foreach ($local in $localRoots) {
            if (-not $local) { continue }
            foreach ($layout in @(@('Python', 'pythoncore-*'), @('Programs\Python', 'Python*'))) {
                $directory = [IO.Path]::Combine($local, $layout[0])
                if ([IO.Directory]::Exists($directory)) {
                    foreach ($installed in [IO.Directory]::GetDirectories($directory, $layout[1])) {
                        $candidates.Add([IO.Path]::Combine($installed, 'python.exe'))
                    }
                }
            }
        }
        foreach ($candidate in $candidates) {
            if (Test-NativePython $candidate) { $python = $candidate; break }
        }
        if (-not $python) { throw 'Existing native Python >=3.11 is required; WindowsApps aliases were not invoked and no installation was attempted.' }
    }
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $python
    $start.UseShellExecute = $false
    $start.WorkingDirectory = $root
    $childArgs = @('-B', [IO.Path]::Combine($PSScriptRoot, 'setup_env.py')) + $forwarded
    $start.Arguments = ($childArgs | ForEach-Object { ConvertTo-NativeArgument ([string]$_) }) -join ' '
    $process = [Diagnostics.Process]::Start($start)
    $process.WaitForExit()
    $code = $process.ExitCode
    $process.Dispose()
    exit $code
} catch {
    [Console]::Error.WriteLine('[setup-env] ' + $_.Exception.Message)
    exit 1
}
