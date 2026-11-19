; TimeTrack Windows installer (Inno Setup 7, #158).
;
; Built only through build-installer.ps1 next to this file, which stages the
; four release binaries into bin\ and compiles with:
;
;   ISCC /DMyAppVersion=1.2.3 timetrack.iss
;
; A bare `ISCC timetrack.iss` also compiles, stamped 0.0.0, for iterating on
; the script itself.
;
; Per-user install: no UAC prompt, everything under the installing user's own
; profile. The program goes to %LOCALAPPDATA%\Programs\TimeTrack while user
; data lives in %LOCALAPPDATA%\timetrack, which no section below touches --
; install, upgrade, repair and uninstall all preserve it by construction.
;
; The service is NOT registered as a Windows service: any client auto-spawns
; the sibling timetrack-service.exe when the named pipe does not answer
; (REQUIREMENTS §16, ADR 0013), so there is nothing to register and nothing
; to leave dangling. The tray's logon autostart is an HKCU Run value owned
; by this installer and removed with it (see [Registry] and [Code]).

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#define MyAppName "TimeTrack"
#define MyAppPublisher "sequ-r"
#define MyAppURL "https://github.com/sequ-r/timetrack"

[Setup]
; One stable identity for install/upgrade/uninstall: a reinstall with the
; same AppId upgrades in place, and Add/Remove Programs shows one entry.
AppId={{917E7F82-CA7D-4EF0-821D-62FAFD09CA68}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
VersionInfoVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
; Per-user: {autopf} resolves to %LOCALAPPDATA%\Programs and no elevation
; is ever requested. PrivilegesRequiredOverridesAllowed keeps the dialog
; available so an admin can still choose all-users explicitly.
DefaultDirName={autopf}\{#MyAppName}
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
MinVersion=10.0
ArchitecturesAllowed=x64compatible
OutputDir=Output
OutputBaseFilename=TimeTrack-Setup-{#MyAppVersion}
SetupIconFile=timetrack.ico
UninstallDisplayIcon={app}\timetrack.ico
Compression=lzma2/ultra64
SolidCompression=yes
; The user-PATH edit in [Code] needs the environment broadcast.
ChangesEnvironment=yes
; A running GUI/tray holds its .exe open and would otherwise fail the
; upgrade copy; Inno asks to close them first.
CloseApplications=yes
RestartApplications=no
WizardStyle=modern
DisableProgramGroupPage=auto

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; \
  GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
; checkedonce: on for a fresh install, keeps the user's earlier choice on
; upgrade or reinstall.
Name: "autostart"; Description: "Start the TimeTrack tray at logon"; \
  GroupDescription: "Startup:"; Flags: checkedonce
Name: "addtopath"; Description: "Add TimeTrack to PATH (the timetrack CLI)"; \
  GroupDescription: "Command line:"; Flags: checkedonce

[Files]
; ignoreversion: reinstall/upgrade always overwrites with the bundled build,
; and repair is just re-running the installer. The .ico ships so the
; shortcuts below have an icon (the .exes carry none yet -- embedding one
; needs a build.rs, which is Rust-side future work, not packaging).
Source: "bin\timetrack-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "bin\timetrack-service.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "bin\timetrack.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "bin\timetrack-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "timetrack.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\timetrack-gui.exe"; \
  IconFilename: "{app}\timetrack.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\timetrack-gui.exe"; \
  IconFilename: "{app}\timetrack.ico"; Tasks: desktopicon

[Registry]
; Tray autostart, owned by this install: uninsdeletevalue removes it on
; uninstall, and the [Code] below removes it when the task is unchecked on a
; reinstall, so no dangling logon entry can survive either path.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
  ValueType: string; ValueName: "TimeTrack"; \
  ValueData: """{app}\timetrack-tray.exe"""; Flags: uninsdeletevalue; \
  Tasks: autostart

[Run]
Filename: "{app}\timetrack-gui.exe"; \
  Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; \
  Flags: nowait postinstall skipifsilent

[Code]
// User-PATH handling. Inno has no built-in PATH editor, so the addtopath
// task is a registry edit on HKCU\Environment\Path plus the broadcast that
// ChangesEnvironment=yes already sends. Both directions match on the exact
// {app} string, so an unrelated entry is never touched.

function AppDir(): String;
begin
  Result := ExpandConstant('{app}');
end;

procedure AddUserPath();
var
  Path: String;
begin
  if RegQueryStringValue(HKCU, 'Environment', 'Path', Path) and (Path <> '') then
  begin
    if Pos(';' + AppDir() + ';', ';' + Path + ';') = 0 then
      RegWriteStringValue(HKCU, 'Environment', 'Path', Path + ';' + AppDir());
  end
  else
    RegWriteStringValue(HKCU, 'Environment', 'Path', AppDir());
end;

procedure RemoveUserPath();
var
  Path, Entry: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, 'Environment', 'Path', Path) then
    exit;
  Entry := AppDir();
  Path := ';' + Path + ';';
  P := Pos(';' + Entry + ';', Path);
  while P > 0 do
  begin
    Delete(Path, P, Length(Entry) + 1);
    P := Pos(';' + Entry + ';', Path);
  end;
  if (Length(Path) > 1) and (Path[1] = ';') then
    Delete(Path, 1, 1);
  if (Length(Path) > 0) and (Path[Length(Path)] = ';') then
    Delete(Path, Length(Path), 1);
  if Path = '' then
    RegDeleteValue(HKCU, 'Environment', 'Path')
  else
    RegWriteStringValue(HKCU, 'Environment', 'Path', Path);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    if WizardIsTaskSelected('addtopath') then
      AddUserPath()
    else
      RemoveUserPath();
    // Unchecking autostart on a reinstall must clear the value the [Registry]
    // section wrote on the earlier install.
    if not WizardIsTaskSelected('autostart') then
      RegDeleteValue(
        HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'TimeTrack');
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    RemoveUserPath();
  // The HKCU Run value is removed by its uninsdeletevalue flag; the Start
  // Menu folder, desktop icon and {app} itself are removed by the uninstaller
  // as things it installed. %LOCALAPPDATA%\timetrack is none of those and is
  // deliberately left alone.
end;
