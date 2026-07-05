; Eir NSIS installer hooks
; Called by the Tauri-generated NSIS installer at install/uninstall time.
; The installer runs with administrator privileges (installMode = perMachine).

!macro NSIS_HOOK_PREINSTALL
  ; Stop the running service BEFORE files are written — Windows can't replace
  ; eir-svc.exe while it's running (this is what broke auto-updates). `sc stop`
  ; only issues the stop control and returns immediately; the service itself can
  ; take up to ~30s to drain an in-flight fix before it exits (its SCM wait_hint is
  ; 35s). A fixed short Sleep would elapse while the old process still holds the exe
  ; handle, so poll for STOPPED up to ~40s instead.
  ExecWait 'sc stop EirSvc'
  Push $R0
  Push $R1
  Push $R2
  StrCpy $R0 0
  wait_stopped_loop:
    ; Early-exit once the service reports STOPPED. The "STOPPED" text is localized on
    ; non-English Windows, so on those systems this never matches and we simply wait
    ; the full bounded window — which is the safe outcome anyway (give the drain time).
    nsExec::Exec 'cmd /c sc query EirSvc | find "STOPPED"'
    Pop $R1
    ; StrCmp with the 4th (jump-if-not-equal) arg omitted falls through when not equal.
    StrCmp $R1 "0" wait_stopped_done
    ; Or the service no longer exists (sc query returns 1060) — nothing to wait for.
    nsExec::Exec 'sc query EirSvc'
    Pop $R2
    StrCmp $R2 "1060" wait_stopped_done
    Sleep 2000
    IntOp $R0 $R0 + 1
    IntCmp $R0 20 wait_stopped_done wait_stopped_loop wait_stopped_done
  wait_stopped_done:
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Tear down any prior service registration (ignored on fresh install)
  ExecWait 'sc stop EirSvc'
  ExecWait '"$INSTDIR\eir-svc.exe" uninstall'

  ; Seed config.toml from the bundled template on first install, then remove the
  ; template so the install directory contains only the live config. The service
  ; auto-detects the claude session, so the default config works as-is.
  IfFileExists "$INSTDIR\config.toml" +2
    CopyFiles /SILENT "$INSTDIR\config.toml.example" "$INSTDIR\config.toml"
  Delete "$INSTDIR\config.toml.example"

  ; Register and start the service
  ExecWait '"$INSTDIR\eir-svc.exe" install'
  ExecWait 'sc start EirSvc'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Stop and unregister before the installer removes the binary
  ExecWait 'sc stop EirSvc'
  ExecWait '"$INSTDIR\eir-svc.exe" uninstall'

  ; Leave no trace (Sysinternals principle): the autostart entry that tauri-plugin-
  ; autostart writes to the current user's Run key points at $INSTDIR\eir.exe, which
  ; is about to be deleted. If left behind it fails at every login. Remove it and the
  ; StartupApproved override, plus the per-user config dir. Note: a perMachine
  ; uninstaller runs in the *uninstalling* user's HKCU, so on a multi-user machine
  ; another user's Run value can't be reached from here — acceptable for a single-user
  ; tool. ($APPDATA = %APPDATA% = Roaming, which is where Tauri stores app config.)
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Eir"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "Eir"
  RMDir /r "$APPDATA\co.swatto.eir"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
