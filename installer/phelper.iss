; phelper Windows installer.
;
; The installer contains one release executable.  PawnIO modules, the app
; icon, and GPUI resources are embedded by the application build; no assets
; directory is installed beside the exe.

#ifndef MyAppVersion
#define MyAppVersion "0.1.0"
#endif

#ifndef BuildDir
#define BuildDir "..\target\release"
#endif

#define MyAppName "phelper"
#define MyAppPublisher "phelper"
#define MyAppExeName "phelper-desktop.exe"

[Setup]
AppId={{8CDE6A93-8C13-4B94-9B54-5E5F6A27C9A1}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\phelper
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64
ArchitecturesInstallIn64BitMode=x64
OutputDir=..\dist
OutputBaseFilename=phelper-Setup-{#MyAppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#BuildDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Comment: "HP OMEN performance control"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

; User settings, profiles, journals, and logs live under %LOCALAPPDATA%\phelper
; and are intentionally preserved by uninstall.
