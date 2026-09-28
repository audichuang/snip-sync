; Inno Setup script for the native snip-sync desktop app (unsigned, per-user).
; Built by scripts/package_native.sh, which passes:
;   /DAppVersion=X.Y.Z /DSourceExe=<path to snip-desktop-native.exe> /DIconFile=<icon.ico>
;   /DLicenseDir=<folder of third-party license texts>

#ifndef AppVersion
  #error AppVersion must be defined
#endif
#ifndef SourceExe
  #error SourceExe must be defined
#endif
#ifndef IconFile
  #error IconFile must be defined
#endif
#ifndef LicenseDir
  #error LicenseDir must be defined
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
Source: "{#LicenseDir}\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\snip-sync"; Filename: "{app}\snip-desktop-native.exe"
Name: "{autodesktop}\snip-sync"; Filename: "{app}\snip-desktop-native.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\snip-desktop-native.exe"; Description: "{cm:LaunchProgram,snip-sync}"; Flags: nowait postinstall skipifsilent

[Code]
// The v0.2.x Tauri build installed per-user with NSIS into
// %LOCALAPPDATA%\snip-sync under this uninstall key. Offer to remove it so
// the two apps (same Start menu name) do not coexist.
const
  TauriUninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\snip-sync';

var
  TauriLeftInstalled: Boolean;

function TauriUninstaller(): String;
var
  S: String;
begin
  Result := '';
  if RegQueryStringValue(HKCU, TauriUninstallKey, 'UninstallString', S) then
    Result := RemoveQuotes(S);
  if (Result = '') or not FileExists(Result) then
    Result := ExpandConstant('{localappdata}\snip-sync\uninstall.exe');
  if not FileExists(Result) then
    Result := '';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Uninst, Dir: String;
  Code: Integer;
begin
  Result := '';
  Uninst := TauriUninstaller();
  if Uninst = '' then
    Exit;
  Dir := ExtractFileDir(Uninst);
  // Silent installs default to No: never remove software unasked.
  if SuppressibleMsgBox('An older snip-sync (the Tauri-based v0.2.x) is installed in' + #13#10 + Dir + #13#10#13#10 +
      'Uninstall it now? Otherwise both versions stay installed and you can remove the old one later in Settings > Apps.',
      mbConfirmation, MB_YESNO, IDNO) <> IDYES then
  begin
    TauriLeftInstalled := True;
    Exit;
  end;
  // "_?=" runs the NSIS uninstaller in place so Exec can wait for it.
  if Exec(Uninst, '/S _?=' + Dir, '', SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0) then
  begin
    // In-place mode leaves the uninstaller itself behind.
    DeleteFile(Uninst);
    RemoveDir(Dir);
  end
  else
  begin
    TauriLeftInstalled := True;
    SuppressibleMsgBox('Could not uninstall the older snip-sync (exit code ' + IntToStr(Code) + ').' + #13#10 +
      'Remove it later in Settings > Apps.', mbError, MB_OK, IDOK);
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = wpFinished) and TauriLeftInstalled then
    WizardForm.FinishedLabel.Caption := WizardForm.FinishedLabel.Caption + #13#10#13#10 +
      'The older Tauri-based snip-sync is still installed. Remove it in Settings > Apps if you no longer need it.';
end;
