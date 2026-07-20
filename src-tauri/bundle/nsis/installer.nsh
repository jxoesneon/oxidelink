; OxideLink NSIS installer hooks
; Optional HidHide/ViGEmBus component page + driver install logic.
; Placeholder installers (0-byte .exe) are skipped automatically.

!include "Sections.nsh"

; Global variables for command-line overrides and file size checks.
Var OxidelinkHidHideForce
Var OxidelinkViGEmForce
Var OxidelinkDriverDir
Var OxidelinkFileSize

; Insert an optional component page. Because Tauri includes this file before its
; built-in page macros, the page appears before the Welcome page. If you need the
; standard order (Welcome -> License -> Components -> Directory ...), use a custom
; NSIS template and set bundle.windows.nsis.template.
!insertmacro MUI_PAGE_COMPONENTS

Section "HidHide driver" SecHidHide
  ; Selection is read in NSIS_HOOK_POSTINSTALL.
SectionEnd

Section /o "ViGEmBus driver" SecViGEm
  ; Selection is read in NSIS_HOOK_POSTINSTALL.
SectionEnd

!macro NSIS_HOOK_POSTINSTALL

  ; Parse silent/passive command-line flags.
  ClearErrors
  ${GetOptions} $CMDLINE "/HIDHIDE" $OxidelinkHidHideForce
  ${IfNot} ${Errors}
    StrCpy $OxidelinkHidHideForce 1
  ${Else}
    StrCpy $OxidelinkHidHideForce 0
  ${EndIf}

  ClearErrors
  ${GetOptions} $CMDLINE "/VIGEM" $OxidelinkViGEmForce
  ${IfNot} ${Errors}
    StrCpy $OxidelinkViGEmForce 1
  ${Else}
    StrCpy $OxidelinkViGEmForce 0
  ${EndIf}

  ; Path where Tauri copies bundled driver installers.
  ; (configured in tauri.conf.json bundle.resources)
  StrCpy $OxidelinkDriverDir "$INSTDIR\resources\drivers"

  ; ----- HidHide -----
  SectionGetFlags ${SecHidHide} $0
  IntOp $0 $0 & ${SF_SELECTED}
  ${If} $0 == ${SF_SELECTED}
  ${OrIf} $OxidelinkHidHideForce == 1
    ClearErrors
    FileOpen $R9 "$OxidelinkDriverDir\HidHideInstaller.exe" r
    ${IfNot} ${Errors}
      FileSeek $R9 0 END $OxidelinkFileSize
      FileClose $R9
      ${If} $OxidelinkFileSize > 0
        DetailPrint "Installing HidHide driver..."
        ExecWait '"$OxidelinkDriverDir\HidHideInstaller.exe" /S' $R7
        ${If} $R7 == 0
          DetailPrint "HidHide installed successfully."
        ${Else}
          MessageBox MB_ICONEXCLAMATION|MB_OK "HidHide installation failed (exit $R7). Some controller features may not work."
        ${EndIf}
      ${Else}
        DetailPrint "HidHide installer is a 0-byte placeholder; skipping."
      ${EndIf}
    ${Else}
      DetailPrint "HidHide installer not bundled; skipping."
    ${EndIf}
  ${EndIf}

  ; ----- ViGEmBus -----
  SectionGetFlags ${SecViGEm} $0
  IntOp $0 $0 & ${SF_SELECTED}
  ${If} $0 == ${SF_SELECTED}
  ${OrIf} $OxidelinkViGEmForce == 1
    ClearErrors
    FileOpen $R9 "$OxidelinkDriverDir\ViGEmBusSetup.exe" r
    ${IfNot} ${Errors}
      FileSeek $R9 0 END $OxidelinkFileSize
      FileClose $R9
      ${If} $OxidelinkFileSize > 0
        DetailPrint "Installing ViGEmBus driver..."
        ExecWait '"$OxidelinkDriverDir\ViGEmBusSetup.exe" /S' $R7
        ${If} $R7 == 0
          DetailPrint "ViGEmBus installed successfully."
        ${Else}
          MessageBox MB_ICONEXCLAMATION|MB_OK "ViGEmBus installation failed (exit $R7). Some controller features may not work."
        ${EndIf}
      ${Else}
        DetailPrint "ViGEmBus installer is a 0-byte placeholder; skipping."
      ${EndIf}
    ${Else}
      DetailPrint "ViGEmBus installer not bundled; skipping."
    ${EndIf}
  ${EndIf}

  ; Ensure start-menu and desktop shortcuts exist. Tauri already creates these in
  ; normal GUI mode; this is a fallback for silent/passive installs.
  ${If} $UpdateMode <> 1
  ${AndIf} $NoShortcutMode <> 1
    CreateDirectory "$SMPROGRAMS\${PRODUCTNAME}"
    CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    ; Desktop shortcut only for silent/passive installs where the finish page is skipped.
    ${If} $PassiveMode = 1
    ${OrIf} ${Silent}
      CreateShortcut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
    ${EndIf}
  ${EndIf}

!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ; Clean up bundled driver installers if anything remains.
  Delete "$INSTDIR\resources\drivers\HidHideInstaller.exe"
  Delete "$INSTDIR\resources\drivers\ViGEmBusSetup.exe"
  RMDir "$INSTDIR\resources\drivers"
  RMDir "$INSTDIR\resources"
!macroend
