; Sentry NSIS installer hooks
; Called by the Tauri-generated NSIS installer at install/uninstall time.
; The installer runs with administrator privileges (installMode = perMachine).

!macro NSIS_HOOK_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Tear down any prior service registration (ignored on fresh install)
  ExecWait 'sc stop SentrySvc'
  ExecWait '"$INSTDIR\sentry-svc.exe" uninstall'

  ; Register the service and start it
  ExecWait '"$INSTDIR\sentry-svc.exe" install'
  ExecWait 'sc start SentrySvc'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Stop and unregister before the installer removes the binary
  ExecWait 'sc stop SentrySvc'
  ExecWait '"$INSTDIR\sentry-svc.exe" uninstall'
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
