; Installeur Windows de RIP Discord (NSIS).
; Construit par .github/workflows/release.yml :
;   makensis /DVERSION=0.1.0 /DEXE=chemin\rip-discord.exe /DOUTFILE=RIP-Discord-Setup.exe installer\rip-discord.nsi

Unicode true
!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !define VERSION "0.0.0"
!endif
!ifndef EXE
  !define EXE "..\target\release\rip-discord.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "RIP-Discord-Setup.exe"
!endif

!define APP "RIP Discord"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\RIPDiscord"

Name "${APP}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APP}"
InstallDirRegKey HKLM "Software\${APP}" "InstallDir"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "${APP} ${VERSION}"

!define MUI_ICON "..\assets\icon.ico"
!define MUI_UNICON "..\assets\icon.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\rip-discord.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Lancer ${APP}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "French"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=${LANG_FRENCH} "ProductName" "${APP}"
VIAddVersionKey /LANG=${LANG_FRENCH} "FileDescription" "Installeur ${APP}"
VIAddVersionKey /LANG=${LANG_FRENCH} "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_FRENCH} "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=${LANG_FRENCH} "LegalCopyright" "MIT OR Apache-2.0"

Section "Installer"
  SetShellVarContext all
  SetOutPath "$INSTDIR"
  ; Mise à jour lancée par l'app : elle se ferme juste après, on attend que
  ; son exe soit libéré pour le remplacer (20 s au plus).
  StrCpy $0 0
  attente:
    IfFileExists "$INSTDIR\rip-discord.exe" 0 libre
    ClearErrors
    FileOpen $1 "$INSTDIR\rip-discord.exe" a
    IfErrors 0 ouvert
    IntOp $0 $0 + 1
    IntCmp $0 100 libre
    Sleep 200
    Goto attente
  ouvert:
    FileClose $1
  libre:
  File "/oname=rip-discord.exe" "${EXE}"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateShortcut "$SMPROGRAMS\${APP}.lnk" "$INSTDIR\rip-discord.exe"
  CreateShortcut "$DESKTOP\${APP}.lnk" "$INSTDIR\rip-discord.exe"

  ; Autorise les connexions entrantes vers l'app (sinon le pare-feu Windows
  ; bloque les amis quand on héberge un salon).
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${APP}"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="${APP}" dir=in action=allow program="$INSTDIR\rip-discord.exe" protocol=UDP enable=yes profile=any'

  WriteRegStr HKLM "Software\${APP}" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APP}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "${APP}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\rip-discord.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
SectionEnd

; « /relance » (mise à jour depuis l'app, avec /S) : rouvre l'app une fois
; installée. Passer par l'explorateur la lance sans les droits administrateur
; de l'installeur.
Function .onInstSuccess
  ${GetParameters} $R0
  ClearErrors
  ${GetOptions} $R0 "/relance" $R1
  IfErrors fin
  Exec '"$WINDIR\explorer.exe" "$INSTDIR\rip-discord.exe"'
  fin:
FunctionEnd

Section "Uninstall"
  SetShellVarContext all
  nsExec::Exec 'netsh advfirewall firewall delete rule name="${APP}"'
  Delete "$SMPROGRAMS\${APP}.lnk"
  Delete "$DESKTOP\${APP}.lnk"
  Delete "$INSTDIR\rip-discord.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "Software\${APP}"
SectionEnd
