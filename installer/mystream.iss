; MyStream installer (Inno Setup 6). Built by CI:
;   iscc /DAppVersion=x.y.z installer\mystream.iss
; Installs per machine, adds a firewall rule (direct P2P connections without
; the Windows prompt) and lets users modify the install folder so the in-app
; updater can replace mystream.exe without admin rights.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{8C1B2F4E-5D7A-4B7E-9A51-3E2C6F0D9A11}
AppName=MyStream
AppVersion={#AppVersion}
AppVerName=MyStream {#AppVersion}
AppPublisher=MyStream contributors
AppPublisherURL=https://github.com/williamloree/mystream
DefaultDirName={autopf}\MyStream
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=admin
OutputDir=..\target\installer
OutputBaseFilename=MyStream-Setup
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\icon.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19041
CloseApplications=force

[Languages]
Name: "french"; MessagesFile: "compiler:Languages\French.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Dirs]
Name: "{app}"; Permissions: users-modify

[Files]
Source: "..\target\release\mystream.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\assets\icon.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\MyStream"; Filename: "{app}\mystream.exe"; IconFilename: "{app}\icon.ico"
Name: "{autodesktop}\MyStream"; Filename: "{app}\mystream.exe"; IconFilename: "{app}\icon.ico"; Tasks: desktopicon

[Run]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""MyStream"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""MyStream"" dir=in action=allow program=""{app}\mystream.exe"" enable=yes profile=any"; Flags: runhidden; StatusMsg: "Autorisation dans le pare-feu Windows…"
Filename: "{app}\mystream.exe"; Description: "{cm:LaunchProgram,MyStream}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[UninstallRun]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""MyStream"""; Flags: runhidden; RunOnceId: "DelFirewallRule"

[UninstallDelete]
Type: files; Name: "{app}\mystream.exe.old"
Type: files; Name: "{app}\mystream.exe.new"
