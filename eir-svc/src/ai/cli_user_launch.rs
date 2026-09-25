//! Windows CreateProcessAsUser launch for subscription CLIs (private to `cli_user`).
use crate::ai::cli_process::{CliProcessOutput, CLI_OUTPUT_CAP};
use crate::session::active_user_session_id;
use anyhow::{bail, Context, Result};
use std::{ffi::OsStr, io::Read, mem::size_of, os::windows::ffi::OsStrExt};
use tracing::warn;
use windows::{
    core::{PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Security::{
            CreateWellKnownSid, EqualSid, GetTokenInformation, ImpersonateLoggedOnUser,
            RevertToSelf, TokenUser, WinLocalSystemSid, PSID, SECURITY_MAX_SID_SIZE, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            },
            RemoteDesktop::WTSQueryUserToken,
            Threading::{
                CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
                ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
                CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
        UI::Shell::GetUserProfileDirectoryW,
    },
};
struct WinHandle(HANDLE);
impl Drop for WinHandle {
    fn drop(&mut self) {
        if self.0 != HANDLE::default() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}
struct UserImpersonation;
impl UserImpersonation {
    fn new(token: HANDLE) -> Result<Self> {
        unsafe {
            ImpersonateLoggedOnUser(token).context("Impersonate active desktop user")?;
        }
        Ok(Self)
    }
}
impl Drop for UserImpersonation {
    fn drop(&mut self) {
        if let Err(error) = unsafe { RevertToSelf() } {
            warn!(%error, "Failed to end active desktop user impersonation");
        }
    }
}
pub(crate) fn running_as_local_system() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return true;
        }
        let _token = WinHandle(token);
        let mut bytes = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut bytes);
        if bytes == 0 {
            return true;
        }
        let mut info = vec![0u8; bytes as usize];
        if GetTokenInformation(
            token,
            TokenUser,
            Some(info.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
        .is_err()
        {
            return true;
        }
        let user = &*(info.as_ptr().cast::<TOKEN_USER>());
        let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut sid_bytes = SECURITY_MAX_SID_SIZE;
        if CreateWellKnownSid(
            WinLocalSystemSid,
            PSID::default(),
            PSID(sid.as_mut_ptr().cast()),
            &mut sid_bytes,
        )
        .is_err()
        {
            return true;
        }
        EqualSid(user.User.Sid, PSID(sid.as_mut_ptr().cast())).is_ok()
    }
}
fn user_profile_for_token(token: HANDLE) -> Result<String> {
    let mut chars = 0;
    unsafe {
        let _ = GetUserProfileDirectoryW(token, PWSTR::null(), &mut chars);
    }
    if chars == 0 {
        bail!("Could not resolve the active desktop user's profile");
    }
    let mut buffer = vec![0u16; chars as usize];
    unsafe {
        GetUserProfileDirectoryW(token, PWSTR(buffer.as_mut_ptr()), &mut chars)
            .context("Resolve active desktop user profile")?;
    }
    if buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf16(&buffer).context("Decode active desktop user profile")
}
fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
fn quote_windows_arg(value: &str) -> String {
    if !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_whitespace() || "\"&|<>^()!".contains(c))
    {
        return value.to_string();
    }
    let mut quoted = String::from("\"");
    let mut slashes = 0;
    for c in value.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(slashes * 2 + 1));
                quoted.push('"');
                slashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(slashes));
                slashes = 0;
                quoted.push(c);
            }
        }
    }
    quoted.push_str(&"\\".repeat(slashes * 2));
    quoted.push('"');
    quoted
}
pub(super) struct CliRedirections<'a> {
    pub(super) profile: &'a str,
    pub(super) stdin: &'a std::path::Path,
    pub(super) stdout: &'a std::path::Path,
    pub(super) stderr: &'a std::path::Path,
}
pub(super) fn user_cli_command_line(
    application: &str,
    binary: &str,
    args: &[String],
    files: CliRedirections<'_>,
) -> Result<String> {
    let paths = [
        binary.to_string(),
        files.profile.to_string(),
        files.stdin.to_string_lossy().into_owned(),
        files.stdout.to_string_lossy().into_owned(),
        files.stderr.to_string_lossy().into_owned(),
    ];
    if paths
        .iter()
        .chain(args.iter())
        .any(|v| v.chars().any(|c| "\"%\r\n\0".contains(c)))
    {
        bail!("CLI argument contains characters unsafe for Windows redirection");
    }
    let invocation = std::iter::once(binary)
        .chain(args.iter().map(String::as_str))
        .map(quote_windows_arg)
        .collect::<Vec<_>>()
        .join(" ");
    let inner = format!(
        "set \"HOME={}\" && set \"CODEX_HOME={}\\.codex\" && \
         {invocation} <{} >{} 2>{}",
        files.profile,
        files.profile,
        quote_windows_arg(&files.stdin.to_string_lossy()),
        quote_windows_arg(&files.stdout.to_string_lossy()),
        quote_windows_arg(&files.stderr.to_string_lossy())
    );
    Ok(format!(
        "{} /D /Q /V:OFF /S /C \"{inner}\"",
        quote_windows_arg(application)
    ))
}
fn read_file_capped(path: &std::path::Path) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(CLI_OUTPUT_CAP as u64).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
use super::UserCliSpec;

pub(crate) fn run_cli_as_active_user(
    spec: UserCliSpec<'_>,
    args: &[String],
    prompt: &str,
    files: &[(String, Vec<u8>)],
    seq: u64,
) -> Result<CliProcessOutput> {
    let UserCliSpec {
        configured_binary,
        resolve_binary,
        what,
        scratch_prefix,
        workspace_flag,
        workspace_files,
        timeout_ms,
    } = spec;
    let session = active_user_session_id()
        .ok_or_else(|| anyhow::anyhow!("No single active desktop user is available for {what}"))?;
    let mut token = HANDLE::default();
    unsafe {
        WTSQueryUserToken(session, &mut token).context("Get active desktop user token")?;
    }
    let token_guard = WinHandle(token);
    let profile = user_profile_for_token(token)?;
    let binary = {
        let _user = UserImpersonation::new(token)?;
        let binary = resolve_binary(configured_binary, Some(&profile));
        if !std::path::Path::new(&binary).is_file() {
            bail!("{what} was not found for the active desktop user");
        }
        binary
    };
    let workspace = std::path::Path::new(&profile)
        .join("AppData\\Local\\Temp")
        .join(format!("{scratch_prefix}-{}-{seq}", std::process::id()));
    let result = (|| {
        let stdin = workspace.join("stdin.txt");
        let stdout = workspace.join("stdout.txt");
        let stderr = workspace.join("stderr.txt");
        {
            let _user = UserImpersonation::new(token)?;
            std::fs::create_dir_all(&workspace)
                .with_context(|| format!("Create {what} user scratch workspace"))?;
            std::fs::write(&stdin, prompt)?;
            std::fs::File::create(&stdout)?;
            std::fs::File::create(&stderr)?;
            for (name, bytes) in files {
                std::fs::write(workspace.join(name), bytes)?;
            }
            for (name, bytes) in workspace_files(&profile) {
                std::fs::write(workspace.join(&name), bytes)
                    .with_context(|| format!("Write {what} workspace file {name}"))?;
            }
        }
        let application = format!(
            "{}\\System32\\cmd.exe",
            std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
        );
        let mut process_args = args.to_vec();
        if let Some(flag) = workspace_flag {
            process_args.push(flag.to_string());
            process_args.push(workspace.to_string_lossy().into_owned());
        }
        let command_line = user_cli_command_line(
            &application,
            &binary,
            &process_args,
            CliRedirections {
                profile: &profile,
                stdin: &stdin,
                stdout: &stdout,
                stderr: &stderr,
            },
        )
        .with_context(|| format!("Build {what} command line"))?;
        let application_w = wide_null(&application);
        let mut command_w = wide_null(&command_line);
        let workspace_w = wide_null(workspace.as_os_str());
        let mut environment = std::ptr::null_mut();
        unsafe {
            CreateEnvironmentBlock(&mut environment, token, false)
                .context("Create active desktop user environment")?;
        }
        let startup = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let spawned = unsafe {
            CreateProcessAsUserW(
                token,
                PCWSTR(application_w.as_ptr()),
                PWSTR(command_w.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                Some(environment),
                PCWSTR(workspace_w.as_ptr()),
                &startup,
                &mut process,
            )
        };
        unsafe {
            let _ = DestroyEnvironmentBlock(environment);
        }
        spawned.with_context(|| format!("Launch {what} as the active desktop user"))?;
        let process_guard = WinHandle(process.hProcess);
        let thread_guard = WinHandle(process.hThread);
        let job = match unsafe { CreateJobObjectW(None, PCWSTR::null()) } {
            Ok(job) => job,
            Err(error) => {
                unsafe {
                    let _ = TerminateProcess(process.hProcess, 1);
                }
                return Err(error).with_context(|| format!("Create {what} process job"));
            }
        };
        let _job_guard = WinHandle(job);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let job_ready = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .and_then(|()| AssignProcessToJobObject(job, process.hProcess))
        };
        if let Err(error) = job_ready {
            unsafe {
                let _ = TerminateProcess(process.hProcess, 1);
            }
            return Err(error).with_context(|| format!("Contain {what} process tree"));
        }
        if unsafe { ResumeThread(thread_guard.0) } == u32::MAX {
            unsafe {
                let _ = TerminateProcess(process.hProcess, 1);
            }
            bail!("Resume {what} process failed");
        }
        match unsafe { WaitForSingleObject(process.hProcess, timeout_ms) } {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => {
                unsafe {
                    let _ = TerminateProcess(process.hProcess, 1);
                    let _ = WaitForSingleObject(process.hProcess, 5_000);
                }
                bail!("{what} timed out after {}s", timeout_ms / 1_000);
            }
            _ => bail!("Wait for {what} failed"),
        }
        let mut code = 0;
        unsafe {
            GetExitCodeProcess(process_guard.0, &mut code)
                .with_context(|| format!("Read {what} exit code"))?;
        }
        let output = {
            let _user = UserImpersonation::new(token)?;
            CliProcessOutput {
                code,
                stdout: read_file_capped(&stdout)?,
                stderr: read_file_capped(&stderr)?,
            }
        };
        Ok(output)
    })();
    if let Ok(_user) = UserImpersonation::new(token) {
        let _ = std::fs::remove_dir_all(&workspace);
    }
    drop(token_guard);
    result
}
