param([Parameter(Mandatory=$true)][string]$File)
$ErrorActionPreference = 'Stop'
$env:NoDefaultCurrentDirectoryInExePath = '1'
foreach ($name in 'IRIS_WINDOWS_PFX', 'IRIS_WINDOWS_PFX_PASSWORD', 'IRIS_TIMESTAMP_URL') {
    if (-not [Environment]::GetEnvironmentVariable($name)) { throw "Signed release requires $name" }
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

function Invoke-SignTool([string[]]$Arguments) {
    $exe = (Get-Command signtool -CommandType Application -ErrorAction Stop |
        Where-Object { [IO.Path]::IsPathRooted($_.Source) } |
        Select-Object -First 1).Source
    if (-not $exe) { throw 'signtool.exe not found in PATH' }
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
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
