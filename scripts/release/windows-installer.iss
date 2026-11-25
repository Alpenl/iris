; Build with scripts/release/package.py; ISCC 6.x is required.
#ifndef AppVersion
  #error AppVersion is required
#endif
#if Copy(AppVersion, 1, 4) != "0.0."
  #error Only 0.0.x Iris packages are authorized
#endif
#ifndef Payload
  #error Payload is required
#endif
[Setup]
AppId={{D8B2D853-0F4D-490C-B55E-61F686D0C1E8}
AppName=Iris
AppVersion={#AppVersion}
AppPublisher=Alpenl
DefaultDirName={localappdata}\Programs\Iris
DefaultGroupName=Iris
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#Output}
OutputBaseFilename={#Artifact}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\iris.exe
#ifdef SignedRelease
SignTool=iris
SignedUninstaller=yes
SignedUninstallerDir={#SignedUninstallerDir}
#endif
LicenseFile={#LicenseFile}
CloseApplications=yes
RestartApplications=no
ChangesAssociations=yes
[InstallDelete]
Type: files; Name: "{app}\*.dll"
[Files]
Source: "{#Payload}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
[Registry]
Root: HKCU; Subkey: "Software\Classes\Iris.Media"; ValueType: string; ValueData: "Iris media"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Iris.Media\DefaultIcon"; ValueType: string; ValueData: "{app}\iris.exe,0"
Root: HKCU; Subkey: "Software\Classes\Iris.Media\shell\open\command"; ValueType: string; ValueData: """{app}\iris.exe"" ""%1"""
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "Iris"; ValueData: "Software\Iris\Capabilities"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Iris"; Flags: uninsdeletekeyifempty
Root: HKCU; Subkey: "Software\Iris\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "Iris"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Iris\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "Iris media player"
Root: HKCU; Subkey: "Software\Iris\Capabilities"; ValueType: string; ValueName: "ApplicationIcon"; ValueData: "{app}\iris.exe,0"
#define Extensions "mp4 m4v mkv mov webm avi ts m2ts flv wmv mpg mpeg jpg jpeg png webp gif heic heif avif tif tiff bmp mp3 flac wav m4a aac ogg opus alac aiff wma "
#sub RegisterExtension
Root: HKCU; Subkey: "Software\Classes\.{#Copy(Extensions, 1, Pos(" ", Extensions) - 1)}"; Flags: uninsdeletekeyifempty
Root: HKCU; Subkey: "Software\Classes\.{#Copy(Extensions, 1, Pos(" ", Extensions) - 1)}\OpenWithProgids"; ValueType: string; ValueName: "Iris.Media"; ValueData: ""; Flags: uninsdeletevalue uninsdeletekeyifempty
Root: HKCU; Subkey: "Software\Iris\Capabilities\FileAssociations"; ValueType: string; ValueName: ".{#Copy(Extensions, 1, Pos(" ", Extensions) - 1)}"; ValueData: "Iris.Media"
#endsub
#for {0; Len(Extensions) > 0; Extensions = Copy(Extensions, Pos(" ", Extensions) + 1, Len(Extensions))} RegisterExtension
[Icons]
Name: "{group}\Iris"; Filename: "{app}\iris.exe"; WorkingDir: "{app}"
Name: "{userdesktop}\Iris"; Filename: "{app}\iris.exe"; WorkingDir: "{app}"; Tasks: desktopicon
[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
[Run]
Filename: "{app}\iris.exe"; Description: "Launch Iris"; Flags: nowait postinstall skipifsilent
