; AtroxaBot's installer for Windows, made with Inno Setup 6 (https://jrsoftware.org/isinfo.php).
;
;   cargo build --release -p atroxabot
;   iscc /DVersion=1.2.3 packaging\windows\atroxabot.iss
;
; writes target\installer\AtroxaBot-Setup-x86_64.exe. The release pipeline (.github/workflows/ci.yml)
; does the same, with the version of the tag.
;
; It installs for the person who runs it, without asking for an administrator: the app goes to
; %LOCALAPPDATA%\Programs\AtroxaBot, a folder the app can write to, which is how it updates itself
; afterwards (crates/core/src/update.rs swaps atroxabot.exe in place). Uninstalling takes the app away and
; leaves what the user made with it (%USERPROFILE%\.atroxabot).
;
; Two things here are known to the app and must stay as they are:
;   * AppId: the app writes its version into this entry after it updated itself (update.rs, `installed`).
;     Changing it would also make a new install not replace the old one.
;   * AppMutex: the app holds a mutex of this name while it runs (platform.rs, `mark_running`), so setup
;     and the uninstaller ask to quit it first instead of failing on a file that is in use.

#ifndef Version
  #error Pass the version to install: iscc /DVersion=1.2.3 atroxabot.iss
#endif
; The executable to pack, the folder of license texts, and where the installer is written.
#ifndef Exe
  #define Exe "..\..\target\release\atroxabot.exe"
#endif
#ifndef Licenses
  #define Licenses "..\..\licenses"
#endif
#ifndef Output
  #define Output "..\..\target\installer"
#endif

[Setup]
AppId={{6F0C3A52-9D1B-4E7A-B8C4-2A5D7E913F60}
AppName=AtroxaBot
AppVersion={#Version}
AppVerName=AtroxaBot {#Version}
AppPublisher=AtroxaBot
AppPublisherURL=https://github.com/Atroxabot/atroxabot
AppSupportURL=https://github.com/Atroxabot/atroxabot/issues
AppUpdatesURL=https://github.com/Atroxabot/atroxabot/releases
AppMutex=AtroxaBot.Running
DefaultDirName={localappdata}\Programs\AtroxaBot
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; One question at most (a desktop icon), then it installs.
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
CloseApplications=no
UninstallDisplayName=AtroxaBot
UninstallDisplayIcon={app}\atroxabot.exe
SetupIconFile=atroxabot.ico
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
OutputDir={#Output}
OutputBaseFilename=AtroxaBot-Setup-x86_64

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Exe}"; DestDir: "{app}"; DestName: "atroxabot.exe"; Flags: ignoreversion
Source: "{#Licenses}\*"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\AtroxaBot"; Filename: "{app}\atroxabot.exe"
Name: "{autodesktop}\AtroxaBot"; Filename: "{app}\atroxabot.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\atroxabot.exe"; Description: "{cm:LaunchProgram,AtroxaBot}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; What the app may have left beside itself while updating.
Type: files; Name: "{app}\atroxabot.exe.old"
Type: files; Name: "{app}\atroxabot.exe.new"
Type: files; Name: "{app}\atroxabot.exe.download"
Type: dirifempty; Name: "{app}"

[Messages]
; Closing the window keeps AtroxaBot running in the tray, so say how it is quit.
SetupAppRunningError=AtroxaBot is running.%n%nQuit it first: right-click its icon in the tray and choose Quit, or press Ctrl+Q in its window. Then click OK to continue, or Cancel to exit.
UninstallAppRunningError=AtroxaBot is running.%n%nQuit it first: right-click its icon in the tray and choose Quit, or press Ctrl+Q in its window. Then click OK to continue, or Cancel to exit.
