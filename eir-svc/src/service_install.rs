use anyhow::{ensure, Context, Result};
use std::{
    ffi::{OsStr, OsString},
    fs,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Component, Path, PathBuf, Prefix},
    process::Command,
};
use windows::{
    core::{GUID, PCWSTR},
    Win32::{
        Foundation::{
            CloseHandle, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE,
        },
        Storage::FileSystem::{
            CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING,
        },
        System::{Com::CoTaskMemFree, SystemInformation::GetSystemDirectoryW},
        UI::Shell::{
            FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86,
            SHGetKnownFolderPath, KNOWN_FOLDER_FLAG,
        },
    },
};
use windows_service::{
    service::{
        ServiceAccess, ServiceErrorControl, ServiceInfo, ServiceStartType, ServiceState,
        ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const DESCRIPTION: &str = "Autonomous Windows system repair agent powered by AI";

pub fn install_or_update(service_name: &str, display_name: &str) -> Result<()> {
    let executable = validate_current_binary()?;
    let service_info = ServiceInfo {
        name: service_name.into(),
        display_name: display_name.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: executable.clone(),
        launch_arguments: vec![],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("connect to the Windows service manager (run as Administrator)")?;
    let access = ServiceAccess::QUERY_CONFIG
        | ServiceAccess::CHANGE_CONFIG
        | ServiceAccess::QUERY_STATUS
        | ServiceAccess::START;

    let service = match manager.open_service(service_name, access) {
        Ok(service) => {
            let config = service
                .query_config()
                .context("read the existing service configuration")?;
            ensure!(
                local_system_account(config.account_name.as_deref()),
                "refusing to preserve a non-LocalSystem service account"
            );
            let status = service
                .query_status()
                .context("read the existing service status")?;
            ensure!(
                status.current_state == ServiceState::Stopped,
                "the existing service must be fully stopped before it can be updated"
            );
            service
                .change_config(&service_info)
                .context("update the existing service registration")?;
            service
        }
        Err(error) if winapi_error(&error, ERROR_SERVICE_DOES_NOT_EXIST.0) => manager
            .create_service(&service_info, access)
            .context("create the service registration")?,
        Err(error) => return Err(error).context("open the existing service registration"),
    };

    service
        .set_description(DESCRIPTION)
        .context("set the service description")?;
    match service.start(&[] as &[OsString]) {
        Ok(()) => {}
        Err(error) if winapi_error(&error, ERROR_SERVICE_ALREADY_RUNNING.0) => {}
        Err(error) => return Err(error).context("start the service"),
    }

    let config = service
        .query_config()
        .context("verify the installed service configuration")?;
    ensure!(
        config.service_type == ServiceType::OWN_PROCESS
            && config.start_type == ServiceStartType::AutoStart
            && local_system_account(config.account_name.as_deref())
            && configured_path_matches(&config.executable_path, &executable),
        "the service manager did not retain the secure Eir registration"
    );
    let status = service
        .query_status()
        .context("verify the installed service status")?;
    ensure!(
        matches!(
            status.current_state,
            ServiceState::Running | ServiceState::StartPending
        ),
        "the service stopped immediately after installation"
    );
    Ok(())
}

pub fn validate_current_binary() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("resolve the service executable")?;
    ensure!(
        ordinary_disk_path(&executable),
        "the service executable must use a local drive path"
    );

    let roots = program_files_roots()?;
    let root_refs = roots.iter().map(PathBuf::as_path).collect::<Vec<_>>();
    ensure!(
        service_path_is_allowed(&executable, &root_refs),
        "refusing to register a LocalSystem binary outside Program Files: {}",
        executable.display()
    );
    ensure_no_reparse_points(&executable)?;
    ensure_single_link_file(&executable)?;

    let canonical_executable = executable
        .canonicalize()
        .with_context(|| format!("canonicalize {}", executable.display()))?;
    let canonical_roots = roots
        .iter()
        .map(|root| {
            root.canonicalize()
                .with_context(|| format!("canonicalize {}", root.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let canonical_root_refs = canonical_roots
        .iter()
        .map(PathBuf::as_path)
        .collect::<Vec<_>>();
    ensure!(
        service_path_is_allowed(&canonical_executable, &canonical_root_refs),
        "the service executable resolves outside Program Files"
    );
    ensure!(
        canonical_executable.is_file(),
        "the service executable is not a regular file"
    );

    harden_install_tree(&executable)?;
    ensure_no_reparse_points(&executable)?;
    ensure_single_link_file(&executable)?;
    let hardened_executable = executable
        .canonicalize()
        .with_context(|| format!("re-check {}", executable.display()))?;
    ensure!(
        paths_equal(&canonical_executable, &hardened_executable),
        "the service executable changed while its permissions were secured"
    );
    Ok(executable)
}

fn program_files_roots() -> Result<Vec<PathBuf>> {
    let mut roots =
        vec![known_folder_path(&FOLDERID_ProgramFiles)
            .context("resolve the Program Files directory")?];
    for folder in [FOLDERID_ProgramFilesX64, FOLDERID_ProgramFilesX86] {
        if let Ok(path) = known_folder_path(&folder) {
            if !roots.iter().any(|root| paths_equal(root, &path)) {
                roots.push(path);
            }
        }
    }
    Ok(roots)
}

fn known_folder_path(folder: &GUID) -> Result<PathBuf> {
    // SAFETY: SHGetKnownFolderPath returns a valid, NUL-terminated CoTaskMem allocation
    // on success. It is copied before being released exactly once with CoTaskMemFree.
    let path = unsafe {
        let raw = SHGetKnownFolderPath(folder, KNOWN_FOLDER_FLAG(0), HANDLE::default())
            .map_err(|error| anyhow::anyhow!(error))?;
        let copied = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast()));
        copied
    }
    .context("decode the Program Files path")?;
    Ok(PathBuf::from(path))
}

fn ensure_no_reparse_points(path: &Path) -> Result<()> {
    for component in path.ancestors().filter(|part| !part.as_os_str().is_empty()) {
        let metadata = fs::symlink_metadata(component)
            .with_context(|| format!("inspect {}", component.display()))?;
        ensure!(
            metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0,
            "service path contains a reparse point: {}",
            component.display()
        );
    }
    Ok(())
}

fn ensure_single_link_file(path: &Path) -> Result<()> {
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            HANDLE::default(),
        )
    }
    .with_context(|| format!("open {} without following links", path.display()))?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let inspected = unsafe { GetFileInformationByHandle(handle, &mut info) }
        .with_context(|| format!("inspect {}", path.display()));
    unsafe {
        let _ = CloseHandle(handle);
    }
    inspected?;
    ensure!(
        info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0
            && info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
            && info.nNumberOfLinks == 1,
        "service binary must be a regular file with exactly one hardlink: {}",
        path.display()
    );
    Ok(())
}

fn harden_install_tree(executable: &Path) -> Result<()> {
    let install_dir = executable
        .parent()
        .context("the service executable has no parent directory")?;
    let icacls = system_directory()?.join("icacls.exe");

    for path in [install_dir, executable] {
        let owner_status = Command::new(&icacls)
            .arg(path)
            .args(["/setowner", "*S-1-5-32-544", "/Q"])
            .status()
            .with_context(|| format!("run {}", icacls.display()))?;
        ensure!(
            owner_status.success(),
            "icacls could not set the protected owner on {} (exit code {:?})",
            path.display(),
            owner_status.code()
        );

        let reset_status = Command::new(&icacls)
            .arg(path)
            .args(["/reset", "/Q"])
            .status()
            .with_context(|| format!("run {}", icacls.display()))?;
        ensure!(
            reset_status.success(),
            "icacls could not reset the protected ACL on {} (exit code {:?})",
            path.display(),
            reset_status.code()
        );
    }
    Ok(())
}

fn system_directory() -> Result<PathBuf> {
    let mut buffer = [0u16; 32_768];
    // SAFETY: the writable buffer is valid for the length passed by the generated
    // binding; the returned length is checked before the initialized slice is read.
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) };
    ensure!(
        length > 0,
        "GetSystemDirectoryW failed: {}",
        std::io::Error::last_os_error()
    );
    let length = usize::try_from(length).context("invalid system-directory length")?;
    ensure!(
        length < buffer.len(),
        "the Windows system-directory path is too long"
    );
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

fn service_path_is_allowed(path: &Path, program_files_roots: &[&Path]) -> bool {
    if !path
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("eir-svc.exe"))
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    let mut components = path.components();
    if !matches!(
        components.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    ) || !matches!(components.next(), Some(Component::RootDir))
    {
        return false;
    }
    program_files_roots
        .iter()
        .any(|root| is_direct_service_child(path, root))
}

fn ordinary_disk_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(
        components.next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    ) && matches!(components.next(), Some(Component::RootDir))
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    let mut path_components = path.components();
    root.components().all(|root_component| {
        path_components
            .next()
            .is_some_and(|path_component| component_equal(path_component, root_component))
    })
}

fn is_direct_service_child(path: &Path, root: &Path) -> bool {
    let mut path_components = path.components();
    if !root.components().all(|root_component| {
        path_components
            .next()
            .is_some_and(|path_component| component_equal(path_component, root_component))
    }) {
        return false;
    }
    matches!(
        (
            path_components.next(),
            path_components.next(),
            path_components.next()
        ),
        (Some(Component::Normal(_)), Some(Component::Normal(_)), None)
    )
}

fn component_equal(left: Component<'_>, right: Component<'_>) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    left.components().count() == right.components().count() && path_starts_with(left, right)
}

fn local_system_account(account: Option<&OsStr>) -> bool {
    account.is_some_and(|account| {
        let account = account.to_string_lossy();
        ["LocalSystem", r".\LocalSystem", r"NT AUTHORITY\SYSTEM"]
            .iter()
            .any(|expected| account.eq_ignore_ascii_case(expected))
    })
}

fn configured_path_matches(configured: &Path, expected: &Path) -> bool {
    let configured = configured.as_os_str().to_string_lossy();
    let configured = configured
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(&configured);
    configured.eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
}

fn winapi_error(error: &windows_service::Error, code: u32) -> bool {
    matches!(
        error,
        windows_service::Error::Winapi(error)
            if error.raw_os_error() == i32::try_from(code).ok()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_binary_must_be_a_plain_eir_executable_under_program_files() {
        let roots = [Path::new(r"C:\Program Files")];

        assert!(service_path_is_allowed(
            Path::new(r"C:\Program Files\Eir\eir-svc.exe"),
            &roots
        ));
        for unsafe_path in [
            r"C:\Users\Alice\Eir\eir-svc.exe",
            r"C:\Program Files Evil\Eir\eir-svc.exe",
            r"C:\Program Files\Eir\..\..\Users\Alice\eir-svc.exe",
            r"C:\Program Files\Writable\Nested\eir-svc.exe",
            r"C:\Program Files\Eir\renamed.exe",
            r"\\server\share\Eir\eir-svc.exe",
        ] {
            assert!(
                !service_path_is_allowed(Path::new(unsafe_path), &roots),
                "{unsafe_path} must not be registered as LocalSystem"
            );
        }
    }
}
