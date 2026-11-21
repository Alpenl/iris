param([Parameter(Mandatory=$true)][string]$File)
$ErrorActionPreference = 'Stop'
foreach ($name in 'IRIS_WINDOWS_PFX', 'IRIS_WINDOWS_PFX_PASSWORD', 'IRIS_TIMESTAMP_URL') {
    if (-not [Environment]::GetEnvironmentVariable($name)) { throw "Signed release requires $name" }
}
& signtool sign /fd SHA256 /td SHA256 /tr $env:IRIS_TIMESTAMP_URL /f $env:IRIS_WINDOWS_PFX /p $env:IRIS_WINDOWS_PFX_PASSWORD $File
if ($LASTEXITCODE -ne 0) { throw "signtool sign failed: $LASTEXITCODE" }
& signtool verify /pa /all $File
if ($LASTEXITCODE -ne 0) { throw "signtool verify failed: $LASTEXITCODE" }
