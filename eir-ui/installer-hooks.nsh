; Eir NSIS installer hooks
; Called by the Tauri-generated NSIS installer at install/uninstall time.
; The installer runs with administrator privileges (installMode = perMachine).

Var EirPreviousInstallDir
Var EirRestartServiceOnFailure
Var EirFailureRestartAllowed
Var EirServiceRollbackPath
Var EirStateSourceDir

Function EirRemovePathEntry
  Exch $0
  Push $1
  Delete "$0"
  RMDir "$0"
  IfFileExists "$0" path_entry_unsafe
  IfFileExists "$0\*.*" path_entry_unsafe
  Pop $1
  Pop $0
  Return

  path_entry_unsafe:
  Abort "Eir found an unsafe existing install entry and could not remove it: $0"
FunctionEnd

Function EirSecurePath
  Exch $0
  Push $1
  nsExec::Exec '"$SYSDIR\icacls.exe" "$0" /setowner *S-1-5-32-544 /Q'
  Pop $1
  StrCmp $1 "0" secure_path_owner_ok
  Abort "Eir could not set the protected owner on: $0"
  secure_path_owner_ok:
  nsExec::Exec '"$SYSDIR\icacls.exe" "$0" /reset /Q'
  Pop $1
  StrCmp $1 "0" secure_path_ok
  Abort "Eir could not reset the protected permissions on: $0"
  secure_path_ok:
  Pop $1
  Pop $0
FunctionEnd

Function EirRequireSafeStateFile
  Exch $0
  Push $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_PATH", w "$0") i.r1'
  StrCmp $1 "0" state_environment_failed
  nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "$$p=$$env:EIR_INSTALL_STATE_PATH; try { if ($$p -notmatch '^[A-Za-z]:\\') { exit 2 }; $$i=Get-Item -LiteralPath $$p -Force -ErrorAction Stop; if ($$i.PSIsContainer -or (($$i.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { exit 3 }; $$d=$$i.Directory; while ($$null -ne $$d) { if (($$d.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { exit 4 }; $$d=$$d.Parent }; $$links=@(& '$SYSDIR\fsutil.exe' hardlink list $$p 2>$$null); if ($$LASTEXITCODE -ne 0 -or $$links.Count -ne 1) { exit 5 }; exit 0 } catch { exit 6 }"`
  Pop $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_PATH", p 0)'
  StrCmp $1 "0" state_file_safe
  Abort "Eir refused an unsafe linked or non-local state file: $0"
  state_file_safe:
  Pop $1
  Pop $0
  Return

  state_environment_failed:
  Abort "Eir could not validate its existing state file."
FunctionEnd

Function EirMigrateStateFile
  Exch $0
  Push $1
  Push $2
  Push $3
  Push $4
  Push $5
  StrCpy $1 "$INSTDIR\$0"
  StrCmp $EirStateSourceDir "" state_file_absent
  StrCpy $2 "$EirStateSourceDir\$0"
  IfFileExists "$2" state_file_present state_file_absent

  state_file_present:
  StrCpy $3 "$INSTDIR\.eir-secure-$0"
  Push "$3"
  Call EirRemovePathEntry
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_SOURCE", w "$2") i.r4'
  StrCmp $4 "0" state_environment_failed
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_DEST", w "$3") i.r4'
  StrCmp $4 "0" state_destination_environment_failed
  ; Keep the validated object open without write/delete sharing until its bytes are
  ; flushed to a new file under the already-hardened Program Files root.
  nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "$$s=$$env:EIR_INSTALL_STATE_SOURCE; $$t=$$env:EIR_INSTALL_STATE_DEST; try { if ($$s -notmatch '^[A-Za-z]:\\' -or $$t -notmatch '^[A-Za-z]:\\') { exit 2 }; $$input=[IO.File]::Open($$s,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read); try { $$i=Get-Item -LiteralPath $$s -Force -ErrorAction Stop; if ($$i.PSIsContainer -or (($$i.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0)) { exit 3 }; $$d=$$i.Directory; while ($$null -ne $$d) { if (($$d.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { exit 4 }; $$d=$$d.Parent }; $$links=@(& '$SYSDIR\fsutil.exe' hardlink list $$s 2>$$null); if ($$LASTEXITCODE -ne 0 -or $$links.Count -ne 1) { exit 5 }; $$output=[IO.File]::Open($$t,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None); try { $$input.CopyTo($$output); $$output.Flush($$true) } finally { $$output.Dispose() } } finally { $$input.Dispose() }; exit 0 } catch { exit 6 }"`
  Pop $4
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_SOURCE", p 0)'
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_DEST", p 0)'
  StrCmp $4 "0" state_file_copy_ok
  Goto state_file_copy_failed
  state_file_copy_ok:
  StrCmp $2 $1 state_file_replace
  Push "$1"
  Call EirRemovePathEntry
  state_file_replace:
  System::Call 'kernel32::MoveFileExW(w "$3", w "$1", i 0x9) i.r4'
  StrCmp $4 "0" state_file_replace_failed
  Push "$1"
  Call EirSecurePath
  StrCmp $EirStateSourceDir $INSTDIR state_file_done
  InitPluginsDir
  FileOpen $5 "$PLUGINSDIR\eir-migrated-$0" w
  IfErrors state_marker_failed
  FileClose $5
  Goto state_file_done

  state_file_absent:
  StrCmp $EirStateSourceDir $INSTDIR state_file_done
  Push "$1"
  Call EirRemovePathEntry
  Goto state_file_done

  state_file_copy_failed:
  Abort "Eir could not safely copy existing state: $0"
  state_destination_environment_failed:
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_STATE_SOURCE", p 0)'
  state_environment_failed:
  Abort "Eir could not validate its existing state file."
  state_file_replace_failed:
  Abort "Eir could not atomically secure existing state: $0"
  state_marker_failed:
  Abort "Eir could not record migrated state: $0"

  state_file_done:
  Pop $5
  Pop $4
  Pop $3
  Pop $2
  Pop $1
  Pop $0
FunctionEnd

Function EirCleanupLegacyStateFile
  Exch $0
  Push $1
  StrCmp $EirStateSourceDir "" cleanup_legacy_state_done
  StrCmp $EirStateSourceDir $INSTDIR cleanup_legacy_state_done
  StrCpy $1 "$PLUGINSDIR\eir-migrated-$0"
  IfFileExists "$1" 0 cleanup_legacy_state_done
  Delete /REBOOTOK "$EirStateSourceDir\$0"
  Delete "$1"
  cleanup_legacy_state_done:
  Pop $1
  Pop $0
FunctionEnd

Function EirSecureIfPresent
  Exch $0
  IfFileExists "$0" secure_present_path 0
  Pop $0
  Return
  secure_present_path:
  Push "$0"
  Call EirRequireSafeStateFile
  Push "$0"
  Call EirSecurePath
  Pop $0
FunctionEnd

Function EirRemoveBundleTree
  Exch $0
  Push $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_BUNDLE_TREE", w "$0") i.r1'
  StrCmp $1 "0" bundle_tree_remove_failed
  nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "$$p=$$env:EIR_INSTALL_BUNDLE_TREE; try { try { [IO.File]::Delete($$p) } catch [UnauthorizedAccessException] { [IO.Directory]::Delete($$p, $$true) }; exit 0 } catch { exit 2 }"`
  Pop $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_BUNDLE_TREE", p 0)'
  StrCmp $1 "0" bundle_tree_removed
  bundle_tree_remove_failed:
  Abort "Eir could not safely replace its bundled runtime: $0"
  bundle_tree_removed:
  Pop $1
  Pop $0
FunctionEnd

Function un.EirDeleteTreeWithoutFollowingReparse
  Exch $0
  Push $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_DELETE_PATH", w "$0") i.r1'
  StrCmp $1 "0" safe_tree_delete_failed
  nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "$$p=$$env:EIR_INSTALL_DELETE_PATH; try { try { [IO.File]::Delete($$p) } catch [UnauthorizedAccessException] { [IO.Directory]::Delete($$p, $$true) }; exit 0 } catch { exit 3 }"`
  Pop $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "EIR_INSTALL_DELETE_PATH", p 0)'
  StrCmp $1 "0" safe_tree_delete_done
  safe_tree_delete_failed:
  Abort "Eir could not safely remove its user data: $0"
  safe_tree_delete_done:
  Pop $1
  Pop $0
FunctionEnd

!macro EirRemoveBundleOutput NAME
  Push "$INSTDIR\${NAME}"
  Call EirRemovePathEntry
!macroend

!macro EirRemoveBundleTree NAME
  Push "$INSTDIR\${NAME}"
  Call EirRemoveBundleTree
!macroend

!macro EirMigrateState NAME
  Push "${NAME}"
  Call EirMigrateStateFile
!macroend

!macro EirCleanupLegacyState NAME
  Push "${NAME}"
  Call EirCleanupLegacyStateFile
!macroend

!macro EirSecureInstalledFile NAME
  Push "$INSTDIR\${NAME}"
  Call EirSecureIfPresent
!macroend

Function EirRestartServiceAfterFailedInstall
  StrCmp $EirRestartServiceOnFailure "1" 0 restart_service_done
  StrCpy $EirRestartServiceOnFailure 0
  StrCmp $EirServiceRollbackPath "" restart_service
  Push "$EirServiceRollbackPath"
  Call EirRequireSafeStateFile
  nsExec::Exec '"$SYSDIR\sc.exe" stop EirSvc'
  Pop $0
  nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "try { $$s=Get-Service -Name 'EirSvc' -ErrorAction Stop; if ($$s.Status -ne 'Stopped') { $$s.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(40)) }; exit 0 } catch { exit 1 }"`
  Pop $0
  StrCmp $0 "0" 0 restart_service_done
  Delete "$INSTDIR\eir-svc.exe"
  RMDir "$INSTDIR\eir-svc.exe"
  IfFileExists "$INSTDIR\eir-svc.exe" restart_service_done
  IfFileExists "$INSTDIR\eir-svc.exe\*.*" restart_service_done
  ClearErrors
  CopyFiles /SILENT "$EirServiceRollbackPath" "$INSTDIR\eir-svc.exe"
  IfErrors restart_service_done
  Push "$INSTDIR\eir-svc.exe"
  Call EirSecurePath
  Push "$INSTDIR\eir-svc.exe"
  Call EirRequireSafeStateFile
  Delete "$EirServiceRollbackPath"
  StrCpy $EirServiceRollbackPath ""
  restart_service:
  nsExec::Exec '"$SYSDIR\sc.exe" start EirSvc'
  Pop $0
  restart_service_done:
FunctionEnd

Function .onInstFailed
  Call EirRestartServiceAfterFailedInstall
FunctionEnd

!define MUI_CUSTOMFUNCTION_ABORT EirOnUserAbort
Function EirOnUserAbort
  Call EirRestartServiceAfterFailedInstall
FunctionEnd

!macro NSIS_HOOK_PREINSTALL
  ; Let Tauri close the UI (or let the user cancel) before stopping its service.
  !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  Push $R0
  Push $R1
  Push $R2
  Push $R3
  ReadRegStr $EirPreviousInstallDir SHCTX "${MANUPRODUCTKEY}" ""
  StrCpy $EirRestartServiceOnFailure 0
  StrCpy $EirFailureRestartAllowed 0

  ; A LocalSystem service binary must not live in a user-selected writable directory.
  !if "${ARCH}" == "x64"
    StrCpy $R0 "$PROGRAMFILES64\${PRODUCTNAME}"
  !else if "${ARCH}" == "arm64"
    StrCpy $R0 "$PROGRAMFILES64\${PRODUCTNAME}"
  !else
    StrCpy $R0 "$PROGRAMFILES\${PRODUCTNAME}"
  !endif
  StrCmp $INSTDIR $R0 protected_install_dir
  IfSilent force_protected_dir
  StrCmp $PassiveMode "1" force_protected_dir
  StrCmp $UpdateMode "1" force_protected_dir
  MessageBox MB_OK|MB_ICONINFORMATION "Eir runs a protected Windows service, so it will be installed in Program Files."
  force_protected_dir:
  StrCpy $INSTDIR $R0
  protected_install_dir:
  SetOutPath $INSTDIR
  ; Never traverse a pre-existing tree while elevated. A formerly writable install
  ; may contain links to files elsewhere; lock only the ordinary root, then replace
  ; every path the Tauri template will write.
  System::Call 'kernel32::GetFileAttributesW(w "$INSTDIR") i.r1'
  IntCmp $R1 -1 preinstall_root_unsafe 0 0
  IntOp $R2 $R1 & 0x400
  IntCmp $R2 0 preinstall_root_safe preinstall_root_unsafe preinstall_root_unsafe
  preinstall_root_safe:
  Push "$INSTDIR"
  Call EirSecurePath

  ; Keep the existing registration throughout the copy. If anything later fails,
  ; the callbacks restore the protected previous service binary before restarting it.
  nsExec::Exec '"$SYSDIR\sc.exe" query EirSvc'
  Pop $R1
  StrCmp $R1 "1060" preinstall_no_service
  StrCmp $R1 "0" 0 preinstall_service_query_failed
  ; Auto-restart is safe only for an ordinary in-place Program Files upgrade. A
  ; legacy custom-path registration is preserved but left stopped on failure.
  StrCmp $EirPreviousInstallDir $INSTDIR 0 +2
    StrCpy $EirFailureRestartAllowed 1
  StrCpy $EirRestartServiceOnFailure $EirFailureRestartAllowed
  nsExec::Exec '"$SYSDIR\sc.exe" stop EirSvc'
  Pop $R1
  StrCpy $R2 0
  wait_stopped_loop:
    ; Numeric PowerShell exit status avoids parsing localized `sc query` text.
    nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "exit [int]((Get-Service -Name 'EirSvc' -ErrorAction SilentlyContinue).Status -ne 'Stopped')"`
    Pop $R1
    StrCmp $R1 "0" preinstall_prepare_files
    nsExec::Exec '"$SYSDIR\sc.exe" query EirSvc'
    Pop $R3
    StrCmp $R3 "1060" preinstall_prepare_files
    Sleep 2000
    IntOp $R2 $R2 + 1
    IntCmp $R2 20 preinstall_stop_timeout wait_stopped_loop preinstall_stop_timeout

  preinstall_no_service:
  StrCpy $EirFailureRestartAllowed 1
  Goto preinstall_prepare_files

  preinstall_root_unsafe:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir's Program Files directory is missing or redirects through a reparse point. Installation was cancelled before files were written."

  preinstall_service_query_failed:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir could not inspect its existing Windows service. Installation was cancelled without changing it."

  preinstall_stop_timeout:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir's service did not stop within 40 seconds. Installation was cancelled without replacing its files."

  preinstall_prepare_files:
  StrCpy $EirStateSourceDir $EirPreviousInstallDir
  StrCpy $EirServiceRollbackPath ""
  Push "$INSTDIR\.eir-svc.rollback.exe"
  Call EirRemovePathEntry
  StrCmp $EirRestartServiceOnFailure "1" 0 preinstall_migrate_state
  IfFileExists "$INSTDIR\eir-svc.exe" 0 preinstall_migrate_state
  Push "$INSTDIR\eir-svc.exe"
  Call EirRequireSafeStateFile
  ClearErrors
  CopyFiles /SILENT "$INSTDIR\eir-svc.exe" "$INSTDIR\.eir-svc.rollback.exe"
  IfErrors preinstall_service_backup_failed
  Push "$INSTDIR\.eir-svc.rollback.exe"
  Call EirSecurePath
  Push "$INSTDIR\.eir-svc.rollback.exe"
  Call EirRequireSafeStateFile
  StrCpy $EirServiceRollbackPath "$INSTDIR\.eir-svc.rollback.exe"

  preinstall_migrate_state:
  ; Clone only ordinary, local, single-link state into fresh protected files. A
  ; fresh install deliberately discards any unregistered state planted at INSTDIR.
  !insertmacro EirMigrateState "config.toml"
  !insertmacro EirMigrateState "config.toml.bak"
  !insertmacro EirMigrateState "eir.db"
  !insertmacro EirMigrateState "eir.db-wal"
  !insertmacro EirMigrateState "eir.db-shm"
  !insertmacro EirMigrateState "eir.log"

  ; These are every direct output written by tauri-bundler's NSIS template. Removing
  ; the directory entry first neutralises both symlinks and hardlinks.
  !insertmacro EirRemoveBundleOutput "eir.exe"
  !insertmacro EirRemoveBundleOutput "eir-svc.exe"
  !insertmacro EirRemoveBundleOutput "config.toml.example"
  !insertmacro EirRemoveBundleOutput "policy.toml"
  !insertmacro EirRemoveBundleOutput "uninstall.exe"
  ; Tauri expands the fixed WebView2 runtime into this exact directory. The .NET
  ; directory primitive removes links themselves and never follows directory links.
  !insertmacro EirRemoveBundleTree "Microsoft.WebView2.FixedVersionRuntime.150.0.4078.105.x64"
  Goto preinstall_done

  preinstall_service_backup_failed:
  StrCpy $EirServiceRollbackPath ""
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir could not preserve its previous service binary. Installation was cancelled."

  preinstall_done:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Seed config.toml from the bundled template on first install, then remove the
  ; template so the install directory contains only the live config.
  IfFileExists "$INSTDIR\config.toml" +2
    CopyFiles /SILENT "$INSTDIR\config.toml.example" "$INSTDIR\config.toml"

  ; Verify only the exact files Eir owns. Recursive ACL changes are forbidden here:
  ; an old writable tree could contain a link to an unrelated privileged file.
  Push $R0
  Push "$INSTDIR"
  Call EirSecurePath
  !insertmacro EirSecureInstalledFile "eir.exe"
  !insertmacro EirSecureInstalledFile "eir-svc.exe"
  !insertmacro EirSecureInstalledFile "config.toml.example"
  !insertmacro EirSecureInstalledFile "policy.toml"
  !insertmacro EirSecureInstalledFile "uninstall.exe"
  !insertmacro EirSecureInstalledFile "config.toml"
  !insertmacro EirSecureInstalledFile "config.toml.bak"
  !insertmacro EirSecureInstalledFile "eir.db"
  !insertmacro EirSecureInstalledFile "eir.db-wal"
  !insertmacro EirSecureInstalledFile "eir.db-shm"
  !insertmacro EirSecureInstalledFile "eir.log"
  Delete "$INSTDIR\config.toml.example"

  ; The install verb securely creates or updates the retained registration and starts it.
  StrCpy $EirRestartServiceOnFailure $EirFailureRestartAllowed
  StrCpy $R0 "launch failed"
  ClearErrors
  ExecWait '"$INSTDIR\eir-svc.exe" install' $R0
  IfErrors service_install_failed
  IntCmp $R0 0 service_install_ok
  service_install_failed:
  Abort "Eir could not install and start its Windows service (exit code $R0)."
  service_install_ok:
  StrCpy $EirRestartServiceOnFailure 0
  StrCmp $EirServiceRollbackPath "" +2
    Delete "$EirServiceRollbackPath"

  ; Once the protected copy is running, remove only legacy files that were actually
  ; validated and migrated. Delete removes a link itself rather than following it.
  !insertmacro EirCleanupLegacyState "config.toml"
  !insertmacro EirCleanupLegacyState "config.toml.bak"
  !insertmacro EirCleanupLegacyState "eir.db"
  !insertmacro EirCleanupLegacyState "eir.db-wal"
  !insertmacro EirCleanupLegacyState "eir.db-shm"
  !insertmacro EirCleanupLegacyState "eir.log"

  ; Preserve an existing per-user autostart choice when a legacy install is moved.
  ReadRegStr $R0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
  StrCmp $R0 "" +2
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  Pop $R0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Let the user cancel before the service is stopped.
  !insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"

  Push $R0
  Push $R1
  Push $R2
  nsExec::Exec '"$SYSDIR\sc.exe" query EirSvc'
  Pop $R0
  StrCmp $R0 "1060" service_uninstall_ok
  StrCmp $R0 "0" 0 service_uninstall_query_failed
  nsExec::Exec '"$SYSDIR\sc.exe" stop EirSvc'
  Pop $R0
  StrCpy $R1 0
  uninstall_wait_stopped_loop:
    nsExec::Exec `"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -NonInteractive -Command "exit [int]((Get-Service -Name 'EirSvc' -ErrorAction SilentlyContinue).Status -ne 'Stopped')"`
    Pop $R0
    StrCmp $R0 "0" uninstall_service_stopped
    nsExec::Exec '"$SYSDIR\sc.exe" query EirSvc'
    Pop $R2
    StrCmp $R2 "1060" service_uninstall_ok
    Sleep 2000
    IntOp $R1 $R1 + 1
    IntCmp $R1 20 service_uninstall_timeout uninstall_wait_stopped_loop service_uninstall_timeout

  service_uninstall_query_failed:
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir could not inspect its Windows service. Uninstallation was cancelled without deleting its files."

  service_uninstall_timeout:
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir's service did not stop within 40 seconds. Uninstallation was cancelled without deleting its files."

  uninstall_service_stopped:
  ; Use the trusted system utility rather than executing a potentially legacy,
  ; user-replaceable service binary from the install directory.
  nsExec::Exec '"$SYSDIR\sc.exe" delete EirSvc'
  Pop $R0
  StrCmp $R0 "0" service_uninstall_ok
  StrCmp $R0 "1060" service_uninstall_ok
  Pop $R2
  Pop $R1
  Pop $R0
  Abort "Eir could not unregister its Windows service."

  service_uninstall_ok:
  Pop $R2
  Pop $R1
  Pop $R0

  ; Updates and an unchecked "Delete app data" choice must preserve user state.
  ; When cleanup is selected, handle the exact directories here and clear Tauri's
  ; checkbox state so its later recursive RMDir path can never run.
  StrCmp $UpdateMode "1" preserve_uninstall_data
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Eir"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "Eir"
  StrCmp $DeleteAppDataCheckboxState "1" 0 cleanup_user_data_done
  Push "$APPDATA\${BUNDLEID}"
  Call un.EirDeleteTreeWithoutFollowingReparse
  Push "$LOCALAPPDATA\${BUNDLEID}"
  Call un.EirDeleteTreeWithoutFollowingReparse
  Push "$APPDATA\Eir"
  Call un.EirDeleteTreeWithoutFollowingReparse
  DeleteRegKey SHCTX "${MANUPRODUCTKEY}"
  DeleteRegKey /ifempty SHCTX "${MANUKEY}"
  DeleteRegValue HKCU "${MANUPRODUCTKEY}" "Installer Language"
  DeleteRegKey /ifempty HKCU "${MANUPRODUCTKEY}"
  DeleteRegKey /ifempty HKCU "${MANUKEY}"
  StrCpy $DeleteAppDataCheckboxState 0
  cleanup_user_data_done:
  preserve_uninstall_data:
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
