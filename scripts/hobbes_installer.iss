; Hobbes Windows Installer - Inno Setup Script
; Compile with: ISCC.exe /DVersion="0.9.57" hobbes_installer.iss
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

; WebView2 bootstrapper — installs Edge WebView2 Runtime if not already present.
; Download from: https://developer.microsoft.com/en-us/microsoft-edge/webview2/
; Place MicrosoftEdgeWebview2Setup.exe in scripts/ before building the installer.
#if FileExists("MicrosoftEdgeWebview2Setup.exe")
Source: "MicrosoftEdgeWebview2Setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall
#endif

[Icons]
Name: "{group}\Hobbes"; Filename: "{app}\hobbes.exe"; IconFilename: "{app}\hobbes.exe"
Name: "{group}\Uninstall Hobbes"; Filename: "{uninstallexe}"
Name: "{autodesktop}\Hobbes"; Filename: "{app}\hobbes.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Run]
; Install WebView2 Runtime if bootstrapper was bundled and WebView2 is not already installed.
; The /silent /install flags perform a quiet background install.
#if FileExists("MicrosoftEdgeWebview2Setup.exe")
Filename: "{tmp}\MicrosoftEdgeWebview2Setup.exe"; Parameters: "/silent /install"; StatusMsg: "Installing Microsoft Edge WebView2 Runtime..."; Flags: waituntilterminated; Check: NeedsWebView2
#endif
Filename: "{app}\hobbes.exe"; Description: "Launch Hobbes"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{app}"

[Code]
// Check if WebView2 Runtime is already installed by looking for the registry key.
// Returns True if WebView2 is NOT installed (i.e., we need to install it).
function NeedsWebView2(): Boolean;
var
  Version: String;
begin
  Result := True;
  // Per-machine install
  if RegQueryStringValue(HKLM, 'SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BEF-ED5599CBBF54}', 'pv', Version) then
  begin
    if Version <> '' then
      Result := False;
  end;
  // Per-user install
  if Result then
  begin
    if RegQueryStringValue(HKCU, 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BEF-ED5599CBBF54}', 'pv', Version) then
    begin
      if Version <> '' then
        Result := False;
    end;
  end;
end;
