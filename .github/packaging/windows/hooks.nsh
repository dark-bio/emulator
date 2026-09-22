; Adds the command directory to the user's PATH and removes it on uninstall.
!include LogicLib.nsh
!include WinMessages.nsh
!define ARK_HOOK_DIR "${__FILEDIR__}"

; PowerShell edits the registry without NSIS's string-length limit on existing PATH values
!macro ArkUpdatePath REMOVE
  Push $0
  Push $1
  InitPluginsDir
  File "/oname=$PLUGINSDIR\ark-path.ps1" "${ARK_HOOK_DIR}\path.ps1"
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$PLUGINSDIR\ark-path.ps1" -Directory "$INSTDIR\bin" ${REMOVE}'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "$1"
    MessageBox MB_OK|MB_ICONSTOP "Could not update your PATH. $1" /SD IDOK
    SetErrorLevel 1
    Abort
  ${EndIf}
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  Pop $1
  Pop $0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro ArkUpdatePath ""
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro ArkUpdatePath "-Remove"
!macroend
