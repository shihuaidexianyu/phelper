; phelper Windows installer.
;
; The application icon, PawnIO modules, and GPUI resources are embedded.
; PresentMon is a pinned portable console, installed with its license.

#ifndef MyAppVersion
#define MyAppVersion "0.2.0"
#endif

#ifndef BuildDir
#define BuildDir "..\target\release"
#endif

#ifndef ToolsDir
#define ToolsDir "..\target\installer-support\tools"
#endif

#ifndef VCRuntimeDir
#error VCRuntimeDir must point to the Visual Studio x64 CRT redistributable directory
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
SetupIconFile=..\apps\desktop\assets\phelper.ico
CloseApplications=no
RestartApplications=no
; Let the user exit from the tray so hardware cleanup can finish first.
AppMutex=Global\phelper-desktop-8bab-single-instance,Global\phelper-desktop-8bab-single-instance-read-only,Global\Phelper.HardwareControl.8BAB.v1
UninstallDisplayName={#MyAppName}
UninstallDisplayIcon={app}\{#MyAppExeName}

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#BuildDir}\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#VCRuntimeDir}\vcruntime140.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ToolsDir}\PresentMon-2.5.1-x64.exe"; DestDir: "{app}\tools"; Flags: ignoreversion
Source: "licenses\PresentMon-LICENSE.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion
Source: "..\assets\pawnio\COPYING"; DestDir: "{app}\licenses"; DestName: "PawnIO-COPYING.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Comment: "HP OMEN performance control"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Remove only phelper's fixed current-user logon task. Failure is harmless
; when the user never enabled autostart or already disabled it from the tray.
Filename: "{sys}\schtasks.exe"; Parameters: "/Delete /F /TN phelper-user-logon"; Flags: runhidden

; User settings, profiles, journals, and logs live under %LOCALAPPDATA%\phelper
; and are intentionally preserved by uninstall.
