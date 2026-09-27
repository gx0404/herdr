#ifndef PackageVersion
  #error PackageVersion is required
#endif
#ifndef StageDir
  #error StageDir is required
#endif
#ifndef RepoDir
  #error RepoDir is required
#endif

[Setup]
AppId={{532CEFC3-E286-41A4-B097-631BD705DB76}
AppName=Herdr GX
AppVersion={#PackageVersion}
VersionInfoVersion={#PackageVersion}
VersionInfoTextVersion={#PackageVersion}
VersionInfoProductVersion={#PackageVersion}
AppPublisher=gx0404
AppPublisherURL=https://github.com/gx0404/herdr
DefaultDirName={localappdata}\Programs\Herdr GX
DisableDirPage=yes
UsePreviousAppDir=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
DisableProgramGroupPage=yes
OutputBaseFilename=herdr-gx-{#PackageVersion}-windows-x86_64-setup
LicenseFile={#RepoDir}\LICENSE
UninstallDisplayIcon={app}\herdr.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
AlwaysRestart=no
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Code]
const
  OwnerKey = 'Software\Herdr GX\Installer';
  EnvironmentKey = 'Environment';
  MachineEnvironmentKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment';
  RegSz = 1;
  RegExpandSz = 2;

function RegOpenKeyEx(Root: THandle; SubKey: String; Options, Access: LongWord;
  var Key: THandle): Longint;
  external 'RegOpenKeyExW@advapi32.dll stdcall';
function RegQueryValueSize(Key: THandle; Name: String; Reserved: LongWord;
  var Kind: LongWord; Data: LongWord; var Size: LongWord): Longint;
  external 'RegQueryValueExW@advapi32.dll stdcall';
function RegQueryValueString(Key: THandle; Name: String; Reserved: LongWord;
  var Kind: LongWord; Data: String; var Size: LongWord): Longint;
  external 'RegQueryValueExW@advapi32.dll stdcall';
function RegCloseKey(Key: THandle): Longint;
  external 'RegCloseKey@advapi32.dll stdcall';
function ExpandEnvironmentStrings(Source, Dest: String; Size: LongWord): LongWord;
  external 'ExpandEnvironmentStringsW@kernel32.dll stdcall';
function CreateFile(Name: String; Access, Share: LongWord; Security: LongWord;
  Creation, Attributes: LongWord; Template: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';
function GetFileAttributes(Name: String): LongWord;
  external 'GetFileAttributesW@kernel32.dll stdcall';
function SendEnvironmentMessage(Window: THandle; Msg: LongWord; WParam: THandle;
  LParam: String; Flags, Timeout: LongWord; var MessageResult: THandle): THandle;
  external 'SendMessageTimeoutW@user32.dll stdcall';

function ReadRawPath(Root: THandle; SubKey: String; var Value: String;
  var Kind: LongWord): Boolean;
var
  Key: THandle;
  Size: LongWord;
  Status: Longint;
begin
  Result := False;
  Value := '';
  Kind := RegExpandSz;
  Status := RegOpenKeyEx(Root, SubKey, 0, 1, Key);
  if Status = 2 then exit;
  if Status <> 0 then RaiseException('Cannot read PATH registry key.');
  try
    Size := 0;
    Status := RegQueryValueSize(Key, 'Path', 0, Kind, 0, Size);
    if Status = 2 then exit;
    if (Status <> 0) or ((Kind <> RegSz) and (Kind <> RegExpandSz)) then
      RaiseException('PATH must be REG_SZ or REG_EXPAND_SZ. No environment changes were made.');
    SetLength(Value, (Size div 2) + 1);
    if RegQueryValueString(Key, 'Path', 0, Kind, Value, Size) <> 0 then
      RaiseException('Cannot read raw PATH.');
    SetLength(Value, Size div 2);
    while (Length(Value) > 0) and (Value[Length(Value)] = #0) do
      Delete(Value, Length(Value), 1);
    Result := True;
  finally
    RegCloseKey(Key);
  end;
end;

procedure WriteRawPath(Value: String; Kind: LongWord);
var
  OK: Boolean;
begin
  if Kind = RegSz then
    OK := RegWriteStringValue(HKCU, EnvironmentKey, 'Path', Value)
  else
    OK := RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', Value);
  if not OK then RaiseException('Cannot write user PATH.');
end;

function Expanded(Value: String): String;
var
  Size: LongWord;
begin
  SetLength(Result, 32768);
  Size := ExpandEnvironmentStrings(Value, Result, 32768);
  if (Size = 0) or (Size > 32768) then
    RaiseException('Cannot expand PATH for comparison.');
  SetLength(Result, Size - 1);
end;

function NormalPath(Value: String): String;
begin
  Result := Trim(Value);
  if (Length(Result) >= 2) and (Result[1] = '"') and
     (Result[Length(Result)] = '"') then
    Result := Copy(Result, 2, Length(Result) - 2);
  Result := Expanded(Result);
  StringChangeEx(Result, '/', '\', True);
  while (Length(Result) > 3) and (Result[Length(Result)] = '\') do
    Delete(Result, Length(Result), 1);
  Result := Lowercase(Result);
end;

function HasPath(Value, Directory: String): Boolean;
var
  Parts: TArrayOfString;
  I: Integer;
begin
  Result := False;
  Parts := StringSplit(Value, [';'], stAll);
  for I := 0 to GetArrayLength(Parts) - 1 do
    if NormalPath(Parts[I]) = NormalPath(Directory) then begin
      Result := True;
      exit;
    end;
end;

procedure BroadcastEnvironment();
var
  Response: THandle;
begin
  SendEnvironmentMessage($FFFF, $001A, 0, 'Environment', 2, 5000, Response);
end;

procedure AddOwnedPath();
var
  Raw, Directory, Previous: String;
  Kind: LongWord;
  Existed: Boolean;
begin
  Directory := ExpandConstant('{app}');
  Existed := ReadRawPath(HKCU, EnvironmentKey, Raw, Kind);
  if not HasPath(Raw, Directory) then begin
    Previous := Raw;
    if Raw <> '' then Raw := Raw + ';';
    Raw := Raw + Directory;
    WriteRawPath(Raw, Kind);
    if not RegWriteStringValue(HKCU, OwnerKey, 'AddedPath', Directory) then begin
      WriteRawPath(Previous, Kind);
      RaiseException('Cannot record PATH ownership.');
    end;
    if Existed then RegWriteDWordValue(HKCU, OwnerKey, 'PathExisted', 1)
    else RegWriteDWordValue(HKCU, OwnerKey, 'PathExisted', 0);
  end;
  if not RegWriteStringValue(HKCU, OwnerKey, 'InstalledDir', Directory) then
    RaiseException('Cannot record the Herdr GX installation directory.');
  BroadcastEnvironment();
end;

procedure RemoveOwnedPath();
var
  Raw, Owned, Remaining: String;
  Kind, Existed: LongWord;
  Parts: TArrayOfString;
  I: Integer;
  Removed, First: Boolean;
begin
  if not RegQueryStringValue(HKCU, OwnerKey, 'AddedPath', Owned) then exit;
  if Owned <> ExpandConstant('{app}') then exit;
  if not ReadRawPath(HKCU, EnvironmentKey, Raw, Kind) then exit;
  Parts := StringSplit(Raw, [';'], stAll);
  Remaining := '';
  Removed := False;
  First := True;
  for I := 0 to GetArrayLength(Parts) - 1 do begin
    if (not Removed) and (Parts[I] = Owned) then Removed := True
    else begin
      if not First then Remaining := Remaining + ';';
      Remaining := Remaining + Parts[I];
      First := False;
    end;
  end;
  if Removed then begin
    Existed := 1;
    RegQueryDWordValue(HKCU, OwnerKey, 'PathExisted', Existed);
    if (Remaining = '') and (Existed = 0) then begin
      if not RegDeleteValue(HKCU, EnvironmentKey, 'Path') then
        RaiseException('Cannot remove the owned user PATH value.');
    end else WriteRawPath(Remaining, Kind);
    BroadcastEnvironment();
  end;
end;

function LockedPayload(Directory: String): String;
var
  Find: TFindRec;
  Path, Extension: String;
  Handle: THandle;
begin
  Result := '';
  if DirExists(Directory) and ((GetFileAttributes(Directory) and $400) <> 0) then begin
    Result := Directory + ' (reparse point: refusing to follow it)';
    exit;
  end;
  if not FindFirst(AddBackslash(Directory) + '*', Find) then exit;
  try
    repeat
      if (Find.Name <> '.') and (Find.Name <> '..') then begin
        Path := AddBackslash(Directory) + Find.Name;
        if (Find.Attributes and FILE_ATTRIBUTE_DIRECTORY) <> 0 then begin
          if (Find.Attributes and $400) <> 0 then
            Result := Path + ' (reparse point: refusing to follow it)'
          else Result := LockedPayload(Path);
        end else begin
          Extension := Lowercase(ExtractFileExt(Path));
          if ((Extension = '.exe') or (Extension = '.dll')) and
             (Pos('unins', Lowercase(Find.Name)) <> 1) then begin
            Handle := CreateFile(Path, $40000000, 0, 0, 3, $80, 0);
            if Handle = THandle(-1) then Result := Path
            else CloseHandle(Handle);
          end;
        end;
        if Result <> '' then exit;
      end;
    until not FindNext(Find);
  finally
    FindClose(Find);
  end;
end;

function OccupiedMessage(): String;
var
  Locked: String;
begin
  Result := '';
  Locked := LockedPayload(ExpandConstant('{app}'));
  if Locked <> '' then
    Result := 'Herdr GX files are in use or not writable: ' + Locked + #13#10 +
      'Close your Herdr GX sessions yourself, then retry. No processes will be killed and no replacement will be scheduled for reboot.';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Previous, Raw: String;
  Kind: LongWord;
begin
  Result := '';
  if NormalPath(ExpandConstant('{app}')) <>
     NormalPath(ExpandConstant('{localappdata}\Programs\Herdr GX')) then begin
    Result := 'Herdr GX must use its independent per-user installation directory.';
    exit;
  end;
  if FileExists(ExpandConstant('{app}\herdr.exe')) then
    if (not RegQueryStringValue(HKCU, OwnerKey, 'InstalledDir', Previous)) or
       (NormalPath(Previous) <> NormalPath(ExpandConstant('{app}'))) then begin
      Result := 'The destination contains an unowned Herdr installation. It will not be overwritten.';
      exit;
    end;
  ReadRawPath(HKCU, EnvironmentKey, Raw, Kind);
  Result := OccupiedMessage();
end;

function PathResolutionMessage(): String;
var
  Machine, UserPath, Candidate: String;
  Kind: LongWord;
  Parts, Extensions: TArrayOfString;
  I, J: Integer;
begin
  ReadRawPath(HKLM, MachineEnvironmentKey, Machine, Kind);
  ReadRawPath(HKCU, EnvironmentKey, UserPath, Kind);
  Parts := StringSplit(Machine + ';' + UserPath, [';'], stAll);
  Extensions := StringSplit('.com;.exe;.bat;.cmd;.ps1', [';'], stAll);
  Result := 'Open a new terminal and verify with where.exe herdr and Get-Command herdr. Existing terminal environments are unchanged. Shell aliases/functions may take precedence.';
  for I := 0 to GetArrayLength(Parts) - 1 do
    if Trim(Parts[I]) <> '' then begin
      if NormalPath(Parts[I]) = NormalPath(ExpandConstant('{app}')) then exit;
      for J := 0 to GetArrayLength(Extensions) - 1 do begin
        Candidate := AddBackslash(NormalPath(Parts[I])) + 'herdr' + Extensions[J];
        if FileExists(Candidate) then begin
          Result := 'PATH conflict: ' + Candidate + ' is a command candidate in an earlier registry PATH entry. Confirm your shell resolution; other installations were not changed.' + #13#10 + Result;
          exit;
        end;
      end;
    end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  MessageText: String;
begin
  if CurStep = ssPostInstall then begin
    AddOwnedPath();
    MessageText := PathResolutionMessage();
    Log(MessageText);
    WizardForm.FinishedLabel.Caption := MessageText;
  end;
end;

function InitializeUninstall(): Boolean;
var
  MessageText: String;
begin
  MessageText := OccupiedMessage();
  Result := MessageText = '';
  if not Result then begin
    Log(MessageText);
    if not UninstallSilent then MsgBox(MessageText, mbError, MB_OK);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then begin
    if OccupiedMessage() <> '' then RaiseException(OccupiedMessage());
    RemoveOwnedPath();
    RegDeleteKeyIncludingSubkeys(HKCU, OwnerKey);
  end;
end;
