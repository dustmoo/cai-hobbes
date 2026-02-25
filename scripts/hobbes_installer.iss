; Hobbes Windows Installer - Inno Setup Script
; Compile with: ISCC.exe /DVersion="0.9.50" hobbes_installer.iss
; Download Inno Setup from: https://jrsoftware.org/isdl.php

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
AppName=Hobbes
AppVersion={#Version}
AppPublisher=Clear Mirror LLC
AppPublisherURL=https://clearmirror.ai
AppSupportURL=https://clearmirror.ai
DefaultDirName={autopf}\Hobbes
DefaultGroupName=Hobbes
OutputBaseFilename=hobbes_{#Version}_setup
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\hobbes.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\LICENSE
PrivilegesRequired=lowest

[Files]
; Main executable (renamed from hobbes_VERSION.exe to hobbes.exe on install)
Source: "..\target\release\hobbes_{#Version}.exe"; DestDir: "{app}"; DestName: "hobbes.exe"; Flags: ignoreversion

[Icons]
Name: "{group}\Hobbes"; Filename: "{app}\hobbes.exe"; IconFilename: "{app}\hobbes.exe"
Name: "{group}\Uninstall Hobbes"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Hobbes"; Filename: "{app}\hobbes.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Run]
Filename: "{app}\hobbes.exe"; Description: "Launch Hobbes"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{app}"
