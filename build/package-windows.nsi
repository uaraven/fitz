Unicode true
!include "MUI2.nsh"

!ifndef APP_VERSION
    !error "APP_VERSION must be provided by package-windows.ps1"
!endif
!ifndef BAYER_DLL
    !error "BAYER_DLL must be provided by package-windows.ps1"
!endif

; makensis changes into the script's directory, so anchor paths at the repo root.
!define RELEASE_DIR "${__FILEDIR__}\..\target\release"

Name "FitSmith"
Caption "FitSmith ${APP_VERSION} Setup"
OutFile "${RELEASE_DIR}\bundle\FitSmith-${APP_VERSION}-windows-x64-setup.exe"
InstallDir "$PROGRAMFILES64\FitSmith"
RequestExecutionLevel admin
SetCompressor /SOLID zlib

!define MUI_ABORTWARNING
!define MUI_ICON "${__FILEDIR__}\..\fitsmith\assets\fitsmith.ico"
!define MUI_UNICON "${__FILEDIR__}\..\fitsmith\assets\fitsmith.ico"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Section "FitSmith"
    SetRegView 64
    SetOutPath "$INSTDIR"
    File "${RELEASE_DIR}\fitz.exe"
    File "${RELEASE_DIR}\fitsmith.exe"
    File /oname=bayer.dll "${BAYER_DLL}"

    CreateDirectory "$SMPROGRAMS\FitSmith"
    CreateShortcut "$SMPROGRAMS\FitSmith\FitSmith.lnk" "$INSTDIR\fitsmith.exe"
    CreateShortcut "$SMPROGRAMS\FitSmith\fitz.lnk" "$INSTDIR\fitz.exe"
    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\FitSmith" "DisplayName" "FitSmith"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\FitSmith" "DisplayVersion" "${APP_VERSION}"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\FitSmith" "DisplayIcon" '"$INSTDIR\fitsmith.exe",0'
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\FitSmith" "UninstallString" '"$INSTDIR\Uninstall.exe"'
SectionEnd

Section "Uninstall"
    SetRegView 64
    Delete "$SMPROGRAMS\FitSmith\FitSmith.lnk"
    Delete "$SMPROGRAMS\FitSmith\fitz.lnk"
    RMDir "$SMPROGRAMS\FitSmith"

    Delete "$INSTDIR\fitz.exe"
    Delete "$INSTDIR\fitsmith.exe"
    Delete "$INSTDIR\bayer.dll"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\FitSmith"
SectionEnd
