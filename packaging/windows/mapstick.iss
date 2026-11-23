; Instalator MapSticka dla Windows (Inno Setup 6).
;
; Instalacja dla bieżącego użytkownika, bez uprawnień administratora
; (%LOCALAPPDATA%\Programs\MapStick); kto woli dla wszystkich, wybierze to
; w pierwszym oknie. Wersję podaje workflow wydania w MAPSTICK_VERSION.
;
; Skrót w menu Start niesie ten sam AppUserModelID, którego używają
; powiadomienia (watch.rs) — Windows podpisuje je wtedy nazwą i ikoną
; MapSticka zamiast „Windows PowerShell”.

#define AppVersion GetEnv("MAPSTICK_VERSION")
#if AppVersion == ""
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{8C1F7B2E-5A4D-4E7B-9F3C-2D6A1B0E4F93}
AppName=MapStick
AppVersion={#AppVersion}
AppVerName=MapStick {#AppVersion}
AppPublisher=MikolajQ
AppPublisherURL=https://github.com/MikolajQ/mapstick
AppSupportURL=https://github.com/MikolajQ/mapstick/issues
DefaultDirName={autopf}\MapStick
DefaultGroupName=MapStick
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=MapStick-{#AppVersion}-setup
SetupIconFile=mapstick.ico
UninstallDisplayIcon={app}\mapstick.exe
; Program trzyma ten mutex (instance.rs) — także przy sprawdzaniu w tle.
; Bez tego odinstalowanie przy otwartym oknie zostawiało mapstick.exe.
AppMutex=MapStickRunning
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "polish"; MessagesFile: "compiler:Languages\Polish.isl"
Name: "german"; MessagesFile: "compiler:Languages\German.isl"
Name: "french"; MessagesFile: "compiler:Languages\French.isl"
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\..\target\release\mapstick.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\MapStick"; Filename: "{app}\mapstick.exe"; AppUserModelID: "MapStick"
Name: "{autodesktop}\MapStick"; Filename: "{app}\mapstick.exe"; Tasks: desktopicon

[Registry]
; Wpis, który MapStick zakłada sam przy pierwszym powiadomieniu — tu tylko
; po to, żeby odinstalowanie go sprzątnęło.
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\MapStick"; ValueType: string; ValueName: "DisplayName"; ValueData: "MapStick"; Flags: uninsdeletekey

[Run]
Filename: "{app}\mapstick.exe"; Description: "{cm:LaunchProgram,MapStick}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Bez programu codzienne zadanie tylko by się wywracało.
Filename: "schtasks.exe"; Parameters: "/Delete /TN ""MapStick check"" /F"; Flags: runhidden; RunOnceId: "RemoveCheckTask"
