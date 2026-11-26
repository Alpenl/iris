param(
    [Parameter(Mandatory=$true)][string]$File,
    [string]$SignTool = ''
)
$ErrorActionPreference = 'Stop'
$env:NoDefaultCurrentDirectoryInExePath = '1'
foreach ($name in 'IRIS_WINDOWS_PFX', 'IRIS_WINDOWS_PFX_PASSWORD', 'IRIS_TIMESTAMP_URL') {
    if (-not [Environment]::GetEnvironmentVariable($name)) { throw "Signed release requires $name" }
}

function Test-TrustedRootedPath([string]$Candidate) {
    if ([string]::IsNullOrWhiteSpace($Candidate)) { return $false }
    if (-not [IO.Path]::IsPathRooted($Candidate)) { return $false }
    $root = [IO.Path]::GetPathRoot($Candidate)
    if ([string]::IsNullOrEmpty($root)) { return $false }
    return $root.StartsWith('\\') -or (
        $root.Length -ge 3 -and $root[1] -eq ':' -and ($root[2] -eq '\' -or $root[2] -eq '/')
    )
}

function Resolve-SignTool() {
    $explicit = if (-not [string]::IsNullOrWhiteSpace($SignTool)) {
        $SignTool.Trim()
    } else {
        $envOverride = [Environment]::GetEnvironmentVariable('IRIS_SIGNTOOL')
        if (-not [string]::IsNullOrWhiteSpace($envOverride)) { $envOverride.Trim() } else { '' }
    }
    if ($explicit) {
        if (-not (Test-TrustedRootedPath $explicit) -or -not (Test-Path -LiteralPath $explicit -PathType Leaf)) {
            throw "SignTool must be an existing absolute path to signtool.exe: $explicit"
        }
        return [IO.Path]::GetFullPath($explicit)
    }
    $rawPath = [Environment]::GetEnvironmentVariable('PATH')
    if (-not [string]::IsNullOrEmpty($rawPath)) {
        foreach ($dir in ($rawPath -split [IO.Path]::PathSeparator)) {
            if (-not (Test-TrustedRootedPath $dir)) { continue }
            $candidate = Join-Path $dir 'signtool.exe'
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                return [IO.Path]::GetFullPath($candidate)
            }
        }
    }
    throw 'signtool.exe not found; pass -SignTool, set IRIS_SIGNTOOL, or add the Windows SDK bin directory as an absolute PATH entry'
}

function ConvertTo-Win32Arg([string]$Value) {
    $sb = New-Object System.Text.StringBuilder
    [void]$sb.Append('"')
    $slashes = 0
    foreach ($ch in $Value.ToCharArray()) {
        if ($ch -eq '"') {
            [void]$sb.Append('\', $slashes * 2 + 1)
            [void]$sb.Append('"')
            $slashes = 0
        } elseif ($ch -eq '\') {
            $slashes += 1
        } else {
            if ($slashes -gt 0) { [void]$sb.Append('\', $slashes); $slashes = 0 }
            [void]$sb.Append($ch)
        }
    }
    if ($slashes -gt 0) { [void]$sb.Append('\', $slashes * 2) }
    [void]$sb.Append('"')
    return $sb.ToString()
}

$resolvedSignTool = Resolve-SignTool

function Invoke-SignTool([string[]]$Arguments) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $resolvedSignTool
    $psi.Arguments = (($Arguments | ForEach-Object { ConvertTo-Win32Arg $_ }) -join ' ')
    $psi.UseShellExecute = $false
    $proc = [System.Diagnostics.Process]::Start($psi)
    $proc.WaitForExit()
    return $proc.ExitCode
}

$code = Invoke-SignTool @(
    'sign', '/fd', 'SHA256', '/td', 'SHA256',
    '/tr', [Environment]::GetEnvironmentVariable('IRIS_TIMESTAMP_URL'),
    '/f', [Environment]::GetEnvironmentVariable('IRIS_WINDOWS_PFX'),
    '/p', [Environment]::GetEnvironmentVariable('IRIS_WINDOWS_PFX_PASSWORD'),
    $File
)
if ($code -ne 0) { throw "signtool sign failed: $code" }
$code = Invoke-SignTool @('verify', '/pa', '/all', $File)
if ($code -ne 0) { throw "signtool verify failed: $code" }
