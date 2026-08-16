; Inno Setup script for ED Compass.
;
; Build:  iscc /DAppVersion=0.1.0 installer\ed-compass.iss
; Expects target\release\ed-compass.exe to already exist.

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif

#define AppName    "ED Compass"
#define AppExeName "ed-compass.exe"
#define AppPublisher "A Zimin"

[Setup]
; Never change this GUID. It is how Windows recognises an upgrade as the same
; product rather than installing a second copy alongside the first.
AppId={{AF32677B-610B-4F3F-BA87-3BA79EA4768E}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
VersionInfoVersion={#AppVersion}

; Install per-user by default, into %LocalAppData%\Programs. This asks for no
; administrator rights, so the installer runs without a UAC prompt — and the
; install directory stays writable, which is where the configuration, captures
; and exported spectrograms are kept. Choosing "for all users" in the dialog
; still works; the application falls back to %AppData% when it finds the
; program directory read-only.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog

DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename=ED-Compass-Setup-{#AppVersion}
SetupIconFile=..\assets\ed-compass.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

; 64-bit only: the audio capture is WASAPI on x64 Windows.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md";                   DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "..\LICENSE";                     DestDir: "{app}"; Flags: ignoreversion

[Icons]
; The control panel is the thing you launch. The overlay appears by itself when
; Elite has focus, so it needs no shortcut of its own.
Name: "{group}\{#AppName}";       Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; \
  Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; \
  Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Settings are the user's, and are left alone. These are ours: written by the
; program, meaningless without it, and otherwise left behind as empty folders.
Type: filesandordirs; Name: "{app}\captures"
Type: dirifempty;     Name: "{app}"
