; Inno Setup script for the native snip-sync desktop app (unsigned, per-user).
; Built by scripts/package_native.sh, which passes:
;   /DAppVersion=X.Y.Z /DSourceExe=<path to snip-desktop-native.exe> /DIconFile=<icon.ico>

#ifndef AppVersion
  #error AppVersion must be defined
#endif
#ifndef SourceExe
  #error SourceExe must be defined
#endif
#ifndef IconFile
  #error IconFile must be defined
#endif

[Setup]
; Never change AppId: upgrades and uninstall are keyed on it.
AppId={{0433EAC3-3F19-4C16-B352-19FE51557CF7}
AppName=snip-sync
AppVersion={#AppVersion}
AppVerName=snip-sync {#AppVersion}
AppPublisher=audichuang
AppPublisherURL=https://github.com/audichuang/snip-sync
; Per-user install, no UAC prompt. Kept apart from the Tauri installer's
; %LOCALAPPDATA%\snip-sync so a rollback to the Tauri build does not clash.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\snip-sync
DefaultGroupName=snip-sync
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\snip-desktop-native.exe
UninstallDisplayName=snip-sync
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "snip-desktop-native.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\snip-sync"; Filename: "{app}\snip-desktop-native.exe"
Name: "{autodesktop}\snip-sync"; Filename: "{app}\snip-desktop-native.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\snip-desktop-native.exe"; Description: "{cm:LaunchProgram,snip-sync}"; Flags: nowait postinstall skipifsilent
