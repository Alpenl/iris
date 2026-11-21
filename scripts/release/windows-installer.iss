; Build with scripts/release/package.py; ISCC 6.x is required.
#ifndef AppVersion
  #error AppVersion is required
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
LicenseFile={#LicenseFile}
CloseApplications=yes
RestartApplications=no
[Files]
Source: "{#Payload}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs
[Icons]
Name: "{group}\Iris"; Filename: "{app}\iris.exe"; WorkingDir: "{app}"
Name: "{userdesktop}\Iris"; Filename: "{app}\iris.exe"; WorkingDir: "{app}"; Tasks: desktopicon
[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
[Run]
Filename: "{app}\iris.exe"; Description: "Launch Iris"; Flags: nowait postinstall skipifsilent
