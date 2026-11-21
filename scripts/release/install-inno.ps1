param([string]$Destination = "$env:LOCALAPPDATA\Programs\Inno Setup 6")
$ErrorActionPreference = 'Stop'
$url = 'https://github.com/jrsoftware/issrc/releases/download/is-6_4_3/innosetup-6.4.3.exe'
$expected = 'f3c42116542c4cc57263c5ba6c4feabfc49fe771f2f98a79d2f7628b8762723b'
$download = Join-Path ([IO.Path]::GetTempPath()) 'iris-innosetup-6.4.3.exe'
Invoke-WebRequest -Uri $url -OutFile $download
if ((Get-FileHash -Algorithm SHA256 $download).Hash.ToLowerInvariant() -ne $expected) { throw 'Inno Setup download checksum mismatch' }
$process = Start-Process -FilePath $download -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER', "/DIR=`"$Destination`"") -Wait -PassThru
if ($process.ExitCode -ne 0) { throw "Inno Setup installation failed: $($process.ExitCode)" }
if (-not (Test-Path (Join-Path $Destination 'ISCC.exe'))) { throw 'Inno Setup compiler missing after installation' }
Remove-Item $download
if ($env:GITHUB_PATH) { $Destination | Out-File -FilePath $env:GITHUB_PATH -Encoding utf8 -Append }
Write-Output "ISCC installed: $Destination\ISCC.exe"
