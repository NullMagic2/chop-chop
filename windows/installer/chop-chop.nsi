; Chop Chop Splitter — Windows installer (NSIS 3, Modern UI 2).
;
; Build (from the repository root, after `cargo build --release` in windows/):
;   makensis -DVERSION=1.8.0 -DAPPDIR=windows\target\release -DFFMPEG=path\to\ffmpeg\bin windows\installer\chop-chop.nsi
; APPDIR holds chop-chop.exe; FFMPEG holds ffmpeg.exe, ffprobe.exe and their DLLs.

Unicode true
SetCompressor /SOLID lzma
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "1.8.0"
!endif
!ifndef APPDIR
  !define APPDIR "..\target\x86_64-pc-windows-gnu\release"
!endif
!ifndef FFMPEG
  !define FFMPEG "ffmpeg"
!endif
!ifndef OUTDIR
  !define OUTDIR "."
!endif

!define APPNAME "Chop Chop Splitter"
!define EXE "chop-chop.exe"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ChopChopSplitter"

Name "${APPNAME}"
OutFile "${OUTDIR}\chop-chop-${VERSION}-windows-x64-setup.exe"
InstallDir "$PROGRAMFILES64\${APPNAME}"
InstallDirRegKey HKLM "Software\ChopChopSplitter" "InstallDir"
RequestExecutionLevel admin
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey /LANG=1033 "ProductName" "${APPNAME}"
VIAddVersionKey /LANG=1033 "FileDescription" "${APPNAME} installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "MIT License"

!include "MUI2.nsh"
!include "x64.nsh"

!define MUI_ICON "..\assets\chop-chop.ico"
!define MUI_UNICON "..\assets\chop-chop.ico"
!define MUI_ABORTWARNING
!define MUI_LANGDLL_ALLLANGUAGES
!define MUI_FINISHPAGE_RUN "$INSTDIR\${EXE}"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "..\..\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; Same languages as the app.
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "PortugueseBR"
!insertmacro MUI_LANGUAGE "Spanish"
!insertmacro MUI_LANGUAGE "Greek"
!insertmacro MUI_RESERVEFILE_LANGDLL

LangString SecAppName ${LANG_ENGLISH} "Chop Chop Splitter"
LangString SecAppName ${LANG_PORTUGUESEBR} "Chop Chop Splitter"
LangString SecAppName ${LANG_SPANISH} "Chop Chop Splitter"
LangString SecAppName ${LANG_GREEK} "Chop Chop Splitter"
LangString SecDesktopName ${LANG_ENGLISH} "Desktop shortcut"
LangString SecDesktopName ${LANG_PORTUGUESEBR} "Atalho na área de trabalho"
LangString SecDesktopName ${LANG_SPANISH} "Acceso directo en el escritorio"
LangString SecDesktopName ${LANG_GREEK} "Συντόμευση στην επιφάνεια εργασίας"
LangString SecAppDesc ${LANG_ENGLISH} "The app and the FFmpeg tools it uses to cut videos."
LangString SecAppDesc ${LANG_PORTUGUESEBR} "O aplicativo e as ferramentas do FFmpeg que ele usa para cortar vídeos."
LangString SecAppDesc ${LANG_SPANISH} "La aplicación y las herramientas de FFmpeg que usa para cortar vídeos."
LangString SecAppDesc ${LANG_GREEK} "Η εφαρμογή και τα εργαλεία FFmpeg που χρησιμοποιεί για την αποκοπή βίντεο."
LangString SecDesktopDesc ${LANG_ENGLISH} "Put a Chop Chop Splitter shortcut on the desktop."
LangString SecDesktopDesc ${LANG_PORTUGUESEBR} "Colocar um atalho do Chop Chop Splitter na área de trabalho."
LangString SecDesktopDesc ${LANG_SPANISH} "Poner un acceso directo de Chop Chop Splitter en el escritorio."
LangString SecDesktopDesc ${LANG_GREEK} "Τοποθέτηση συντόμευσης του Chop Chop Splitter στην επιφάνεια εργασίας."

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Chop Chop Splitter requires 64-bit Windows 10 or later."
    Abort
  ${EndIf}
  SetRegView 64
  SetShellVarContext all
  !insertmacro MUI_LANGDLL_DISPLAY
FunctionEnd

Function un.onInit
  SetRegView 64
  SetShellVarContext all
  !insertmacro MUI_UNGETLANGUAGE
FunctionEnd

Section "!$(SecAppName)" SecApp
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${APPDIR}\${EXE}"
  File "..\..\LICENSE"
  ; FFmpeg (GPL build) — the app finds it next to chop-chop.exe.
  File "${FFMPEG}\ffmpeg.exe"
  File "${FFMPEG}\ffprobe.exe"
  File /nonfatal "${FFMPEG}\*.dll"
  SetOutPath "$INSTDIR\licenses"
  File /oname=FFmpeg-LICENSE.txt "${FFMPEG}\..\LICENSE.txt"
  File /oname=Yaru-icons-CC-BY-SA-4.0.txt "..\..\data\icons\LICENSE_CCBYSA"
  SetOutPath "$INSTDIR"

  CreateShortcut "$SMPROGRAMS\${APPNAME}.lnk" "$INSTDIR\${EXE}"

  ; "Open with" for video files.
  WriteRegStr HKLM "Software\Classes\Applications\${EXE}" "FriendlyAppName" "${APPNAME}"
  WriteRegStr HKLM "Software\Classes\Applications\${EXE}\shell\open\command" "" '"$INSTDIR\${EXE}" "%1"'
  !macro OpenWith ext
    WriteRegStr HKLM "Software\Classes\Applications\${EXE}\SupportedTypes" "${ext}" ""
    WriteRegStr HKLM "Software\Classes\${ext}\OpenWithList\${EXE}" "" ""
  !macroend
  !insertmacro OpenWith ".mp4"
  !insertmacro OpenWith ".mkv"
  !insertmacro OpenWith ".mov"
  !insertmacro OpenWith ".webm"
  !insertmacro OpenWith ".avi"
  !insertmacro OpenWith ".m4v"
  !insertmacro OpenWith ".mpg"
  !insertmacro OpenWith ".wmv"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\App Paths\${EXE}" "" "$INSTDIR\${EXE}"

  ; Apps & features entry.
  WriteRegStr HKLM "Software\ChopChopSplitter" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST_KEY}" "Publisher" "NMagic"
  WriteRegStr HKLM "${UNINST_KEY}" "URLInfoAbout" "https://github.com/NullMagic2/chop-chop"
  WriteRegStr HKLM "${UNINST_KEY}" "DisplayIcon" "$INSTDIR\${EXE}"
  WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST_KEY}" "NoRepair" 1
SectionEnd

Section "$(SecDesktopName)" SecDesktop
  CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\${EXE}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecApp} $(SecAppDesc)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(SecDesktopDesc)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$INSTDIR\${EXE}"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\ffprobe.exe"
  Delete "$INSTDIR\*.dll"
  Delete "$INSTDIR\LICENSE"
  RMDir /r "$INSTDIR\licenses"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APPNAME}.lnk"
  Delete "$DESKTOP\${APPNAME}.lnk"
  DeleteRegKey HKLM "Software\Classes\Applications\${EXE}"
  !macro UnOpenWith ext
    DeleteRegKey HKLM "Software\Classes\${ext}\OpenWithList\${EXE}"
  !macroend
  !insertmacro UnOpenWith ".mp4"
  !insertmacro UnOpenWith ".mkv"
  !insertmacro UnOpenWith ".mov"
  !insertmacro UnOpenWith ".webm"
  !insertmacro UnOpenWith ".avi"
  !insertmacro UnOpenWith ".m4v"
  !insertmacro UnOpenWith ".mpg"
  !insertmacro UnOpenWith ".wmv"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\App Paths\${EXE}"
  DeleteRegKey HKLM "${UNINST_KEY}"
  DeleteRegKey HKLM "Software\ChopChopSplitter"
SectionEnd
