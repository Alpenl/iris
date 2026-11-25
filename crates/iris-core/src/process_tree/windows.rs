//! Atomic process-tree launch, shared by legacy plugins and typed tool tasks.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, PipeReader, PipeWriter};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_APPEXECLINK;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

pub(crate) struct Child {
    process: OwnedHandle,
    job: Option<OwnedHandle>,
}

impl Child {
    pub(crate) fn status(&self) -> io::Result<Option<ExitStatus>> {
        let process = self.process.as_raw_handle();
        match unsafe { WaitForSingleObject(process, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                check(unsafe { GetExitCodeProcess(process, &mut code) })?;
                Ok(Some(ExitStatus::from_raw(code)))
            }
            _ => Err(io::Error::last_os_error()),
        }
    }

    pub(crate) fn close_job(&mut self) {
        if self.job.take().is_some() {
            unsafe {
                WaitForSingleObject(self.process.as_raw_handle(), 5_000);
            }
        }
    }
}

fn check(result: i32) -> io::Result<()> {
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn kill_on_close_job() -> io::Result<OwnedHandle> {
    let raw = unsafe { CreateJobObjectW(null(), null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // An unnamed job with default security attributes has one non-inheritable handle.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    check(unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        )
    })?;
    Ok(job)
}

fn terminate_wide(mut text: Vec<u16>) -> io::Result<Vec<u16>> {
    if text.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "embedded NUL in process launch",
        ));
    }
    text.push(0);
    Ok(text)
}

// Match Command's CRT quoting, including the original policy for unquoted wildcard arguments.
fn command_line(cmd: &Command) -> io::Result<Vec<u16>> {
    const QUOTE: u16 = b'"' as u16;
    const SLASH: u16 = b'\\' as u16;
    let mut line = vec![QUOTE];
    line.extend(cmd.get_program().encode_wide());
    line.push(QUOTE);
    for arg in cmd.get_args() {
        line.push(b' ' as u16);
        let quoted = arg.is_empty()
            || arg
                .as_encoded_bytes()
                .iter()
                .any(|b| matches!(b, b' ' | b'\t'));
        if quoted {
            line.push(QUOTE);
        }
        let mut slashes = 0;
        for unit in arg.encode_wide() {
            if unit == QUOTE {
                line.extend(std::iter::repeat_n(SLASH, slashes + 1));
            }
            line.push(unit);
            if unit == SLASH {
                slashes += 1;
            } else {
                slashes = 0;
            }
        }
        if quoted {
            line.extend(std::iter::repeat_n(SLASH, slashes));
            line.push(QUOTE);
        }
    }
    terminate_wide(line)
}

fn system_dir(get: unsafe extern "system" fn(*mut u16, u32) -> u32) -> Option<PathBuf> {
    let mut buffer = [0; 260];
    let len = unsafe { get(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    match len {
        0 => None,
        n if n < buffer.len() => Some(OsString::from_wide(&buffer[..n]).into()),
        _ => {
            let mut buffer = vec![0; len];
            let n = unsafe { get(buffer.as_mut_ptr(), len as u32) } as usize;
            (n > 0 && n < len).then(|| OsString::from_wide(&buffer[..n]).into())
        }
    }
}

fn usable_app_exec_link(data: &[u8]) -> bool {
    fn wide_eq(field: &[u8], ascii: &[u8]) -> bool {
        field.len() == ascii.len() * 2
            && field
                .as_chunks::<2>()
                .0
                .iter()
                .zip(ascii)
                .all(|(pair, ch)| pair[1] == 0 && pair[0].eq_ignore_ascii_case(ch))
    }

    // The header is followed by a 32-bit version and NUL-terminated UTF-16 fields:
    // package family, app ID, executable path, and application type.
    let Some(header) = data.first_chunk::<8>() else {
        return false;
    };
    let tag = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = usize::from(u16::from_le_bytes([header[4], header[5]]));
    if tag != IO_REPARSE_TAG_APPEXECLINK
        || len < 4
        || !len.is_multiple_of(2)
        || data.get(8..12) != Some(&3u32.to_le_bytes())
    {
        return false;
    }
    let Some(mut fields) = data.get(12..8 + len) else {
        return false;
    };
    let mut field = || {
        let end = fields
            .as_chunks::<2>()
            .0
            .iter()
            .position(|pair| *pair == [0, 0])?
            * 2;
        let value = &fields[..end];
        fields = &fields[end + 2..];
        (!value.is_empty()).then_some(value)
    };
    let (Some(package), Some(app), Some(target), Some(_)) = (field(), field(), field(), field())
    else {
        return false;
    };
    let basename = target
        .rchunks_exact(2)
        .position(|pair| pair == [b'\\', 0] || pair == [b'/', 0])
        .map_or(target, |count| &target[target.len() - count * 2..]);
    !(wide_eq(package, b"Microsoft.DesktopAppInstaller_8wekyb3d8bbwe")
        && (wide_eq(
            app,
            b"Microsoft.DesktopAppInstaller_8wekyb3d8bbwe!PythonRedirector",
        ) || wide_eq(basename, b"AppInstallerPythonRedirector.exe")))
}

fn usable_executable(path: &Path) -> bool {
    if path
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() != 0)
    {
        return true;
    }
    if !path.symlink_metadata().is_ok_and(|meta| {
        meta.is_file() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }) {
        return false;
    }
    let Ok(file) = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    else {
        return false;
    };
    let mut data = [0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut read = 0;
    // Inspect the alias itself. Windows still needs the alias path to activate the package.
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            null(),
            0,
            data.as_mut_ptr().cast(),
            MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
            &mut read,
            null_mut(),
        )
    };
    result != 0 && usable_app_exec_link(&data[..read as usize])
}

fn executable(program: &OsStr) -> io::Result<Vec<u16>> {
    let path = Path::new(program);
    if path.is_absolute() {
        return terminate_wide(program.encode_wide().collect());
    }
    // Python comes from absolute search roots, never a relative PATH entry.
    let filename = path.with_extension("exe");
    let paths = env::var_os("PATH").unwrap_or_default();
    let dirs = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_owned))
        .into_iter()
        .chain(std::iter::once_with(|| system_dir(GetSystemDirectoryW)).flatten())
        .chain(std::iter::once_with(|| system_dir(GetWindowsDirectoryW)).flatten())
        .chain(env::split_paths(&paths).filter(|p| p.is_absolute()));
    for dir in dirs {
        let candidate = dir.join(&filename);
        if usable_executable(&candidate) {
            return terminate_wide(candidate.as_os_str().encode_wide().collect());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", filename.display()),
    ))
}

fn environment_block(pairs: impl IntoIterator<Item = (OsString, OsString)>) -> Vec<u16> {
    let mut pairs: Vec<_> = pairs
        .into_iter()
        .map(|(k, v)| (k.encode_wide().collect::<Vec<_>>(), v))
        .collect();
    pairs.sort_unstable_by(|(a, _), (b, _)| {
        let cmp = unsafe {
            CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
        };
        cmp.cmp(&CSTR_EQUAL)
    });
    let mut block = Vec::new();
    for (key, value) in pairs {
        block.extend(key);
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn current_directory(path: &Path) -> io::Result<Vec<u16>> {
    use std::path::{Component, Prefix};
    let mut parts = path.components();
    let ordinary = match parts.next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::VerbatimDisk(disk) => Some(PathBuf::from(format!("{}:\\", char::from(disk)))),
            Prefix::VerbatimUNC(server, share) => {
                let mut root = OsString::from(r"\\");
                root.push(server);
                root.push(r"\");
                root.push(share);
                Some(PathBuf::from(root))
            }
            _ => None,
        },
        _ => None,
    };
    let dir = ordinary.map(|mut dir| {
        for part in parts.filter(|part| !matches!(part, Component::RootDir)) {
            dir.push(part);
        }
        dir
    });
    terminate_wide(
        dir.as_deref()
            .unwrap_or(path)
            .as_os_str()
            .encode_wide()
            .collect(),
    )
}

// The API provides an opaque dynamically-sized structure. Store it in pointer-aligned memory.
struct AttributeList(Vec<usize>);

impl AttributeList {
    fn new(count: u32) -> io::Result<Self> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), count, 0, &mut bytes) };
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || bytes == 0 {
            return Err(error);
        }
        let mut storage = Vec::<usize>::with_capacity(bytes.div_ceil(size_of::<usize>()));
        check(unsafe {
            InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), count, 0, &mut bytes)
        })?;
        Ok(Self(storage))
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast()) };
    }
}

type Launch = (Child, PipeWriter, PipeReader, PipeReader);

/// Command does not expose whether env_clear was called. Tool launches build an
/// explicit environment block instead of accidentally merging inherited values.
pub(super) fn spawn_clean(cmd: &Command) -> io::Result<Launch> {
    let env = environment_block(
        cmd.get_envs()
            .filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned()))),
    );
    create_with_env(cmd, 0, Some(env))
}

fn create_with_env(cmd: &Command, extra_flags: u32, env: Option<Vec<u16>>) -> io::Result<Launch> {
    let exe = executable(cmd.get_program())?;
    let mut line = command_line(cmd)?;
    let dir = cmd.get_current_dir().map(current_directory).transpose()?;
    let job = kill_on_close_job()?;
    let (child_stdin, stdin) = io::pipe()?;
    let (stdout, child_stdout) = io::pipe()?;
    let (stderr, child_stderr) = io::pipe()?;
    let jobs = [job.as_raw_handle()];
    let handles = [
        child_stdin.as_raw_handle(),
        child_stdout.as_raw_handle(),
        child_stderr.as_raw_handle(),
    ];
    // io::pipe creates private handles. Only the three child ends are made inheritable,
    // and HANDLE_LIST excludes every other inheritable handle from this process creation.
    for handle in handles {
        check(unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) })?;
    }
    let mut attributes = AttributeList::new(2)?;
    let list = attributes.0.as_mut_ptr().cast();
    for (key, values) in [
        (PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs.as_slice()),
        (PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles.as_slice()),
    ] {
        // The two arrays and all their handles live until after the attribute list is deleted.
        check(unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                key as usize,
                values.as_ptr().cast(),
                size_of_val(values),
                null_mut(),
                null(),
            )
        })?;
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    [
        startup.StartupInfo.hStdInput,
        startup.StartupInfo.hStdOutput,
        startup.StartupInfo.hStdError,
    ] = handles;
    startup.lpAttributeList = list;
    let mut info = PROCESS_INFORMATION::default();
    check(unsafe {
        CreateProcessW(
            exe.as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | extra_flags,
            env.as_ref().map_or(null(), |e| e.as_ptr().cast()),
            dir.as_ref().map_or(null(), |d| d.as_ptr()),
            (&raw const startup).cast(),
            &mut info,
        )
    })?;
    // Membership and the three inherited handles are already established at CreateProcessW return.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    drop(unsafe { OwnedHandle::from_raw_handle(info.hThread) });
    Ok((
        Child {
            process,
            job: Some(job),
        },
        stdin,
        stdout,
        stderr,
    ))
}

#[cfg(test)]
mod tests {
    fn environment(cmd: &Command) -> Option<Vec<u16>> {
        cmd.get_envs().next()?;
        let inherited = env::vars_os()
            .filter(|(key, _)| !cmd.get_envs().any(|(k, _)| key.eq_ignore_ascii_case(k)));
        let overrides = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned())));
        Some(environment_block(inherited.chain(overrides)))
    }

    pub(crate) fn spawn(cmd: &Command) -> io::Result<Launch> {
        create(cmd, 0)
    }

    fn create(cmd: &Command, extra_flags: u32) -> io::Result<Launch> {
        create_with_env(cmd, extra_flags, environment(cmd))
    }

    use super::*;
    use std::io::{Read, Write};
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{CREATE_SUSPENDED, CreateEventW, TerminateProcess};

    #[test]
    fn environment_is_case_insensitively_sorted_and_double_terminated() {
        assert_eq!(environment_block([]), [0, 0]);
        let block = environment_block([("b".into(), "2".into()), ("A".into(), "1".into())]);
        assert_eq!(String::from_utf16(&block).unwrap(), "A=1\0b=2\0\0");
    }

    #[test]
    fn child_current_dir_uses_normal_dos_and_unc_paths() {
        let dos = current_directory(Path::new(r"\\?\C:\Iris\Plugins")).unwrap();
        let unc = current_directory(Path::new(r"\\?\UNC\server\share\plugins")).unwrap();
        assert_eq!(
            String::from_utf16(&dos[..dos.len() - 1]).unwrap(),
            r"C:\Iris\Plugins"
        );
        assert_eq!(
            String::from_utf16(&unc[..unc.len() - 1]).unwrap(),
            r"\\server\share\plugins"
        );
    }

    #[test]
    fn python_app_aliases_skip_only_the_store_redirector() {
        let alias = |package: &str, entry: &str, executable: &str| {
            let strings = format!("{package}\0{package}!{entry}\0{executable}\0{}\0", '0');
            let fields: Vec<_> = strings.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let mut data = IO_REPARSE_TAG_APPEXECLINK.to_le_bytes().to_vec();
            data.extend_from_slice(&((4 + fields.len()) as u16).to_le_bytes());
            data.extend_from_slice(&[0, 0, 3, 0, 0, 0]);
            data.extend_from_slice(&fields);
            data
        };
        let python = alias(
            "PythonSoftwareFoundation.Python.3.13_qbz5n2kfra8p0",
            "Python",
            r"C:\Program Files\WindowsApps\PythonSoftwareFoundation.Python.3.13\python.exe",
        );
        assert!(usable_app_exec_link(&python));
        assert!(!usable_app_exec_link(&python[..python.len() - 2]));
        let installer = "Microsoft.DesktopAppInstaller_8wekyb3d8bbwe";
        assert!(!usable_app_exec_link(&alias(
            installer,
            "PythonRedirector",
            r"C:\Program Files\WindowsApps\AppInstaller\AppInstallerPythonRedirector.exe",
        )));
        assert!(usable_app_exec_link(&alias(
            installer,
            "WinGet",
            r"C:\WindowsApps\winget.exe"
        )));
        let mut wrong_ver = python.clone();
        wrong_ver[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(!usable_app_exec_link(&wrong_ver));
        assert!(!usable_app_exec_link(&[0; 16]));
    }

    #[test]
    fn ordinary_empty_executable_is_not_a_candidate() {
        let path = env::temp_dir().join(format!("iris-empty-exe-{}.exe", std::process::id()));
        std::fs::File::create_new(&path).unwrap();
        let usable = usable_executable(&path);
        std::fs::remove_file(path).unwrap();
        assert!(!usable);
    }

    #[test]
    fn plugin_is_born_in_its_job_before_first_instruction() {
        let cmd = Command::new(env::current_exe().unwrap());
        let (mut child, ..) = create(&cmd, CREATE_SUSPENDED).unwrap();
        let process = child.process.as_raw_handle();
        let job = child.job.take().unwrap();
        let mut member = 0;
        let query = check(unsafe { IsProcessInJob(process, job.as_raw_handle(), &mut member) });
        // Reproduce the kernel's last-handle close on host death while the primary thread
        // has never run. A missing creation attribute must fail without leaking the fixture.
        drop(job);
        let wait = unsafe { WaitForSingleObject(process, 5_000) };
        if wait != WAIT_OBJECT_0 {
            unsafe { TerminateProcess(process, 1) };
        }
        query.unwrap();
        assert_ne!(member, 0, "new process was outside the exact plugin job");
        assert_eq!(
            wait, WAIT_OBJECT_0,
            "job close did not terminate the suspended process"
        );
    }

    #[test]
    fn plugin_inherits_only_stdio_and_round_trips_arguments() {
        let raw = unsafe { CreateEventW(null(), 1, 0, null()) };
        assert!(!raw.is_null());
        let sentinel = unsafe { OwnedHandle::from_raw_handle(raw) };
        check(unsafe { SetHandleInformation(raw, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) })
            .unwrap();
        let args = [
            "",
            "space and tab\t",
            "尾巴 \\",
            "say\"hello",
            r#"slash\\\"quote"#,
            "*?.txt",
        ];
        let script = r#"import ctypes, json, os, sys
ctypes.windll.kernel32.SetEvent(ctypes.c_void_p(int(sys.argv[1])))
print(json.dumps([sys.argv[2:], sys.stdin.read(), os.getcwd(), os.environ['PYTHONUTF8'], os.environ['PYTHONIOENCODING']]))
print('stderr-ok', file=sys.stderr)
"#;
        let dir = env::temp_dir().canonicalize().unwrap();
        let mut cmd = Command::new("python");
        cmd.args(["-u", "-c", script])
            .arg((raw as usize).to_string())
            .args(args)
            .current_dir(&dir)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8");
        let (child, mut stdin, mut stdout, mut stderr) = spawn(&cmd).unwrap();
        stdin.write_all("pipe✓".as_bytes()).unwrap();
        drop(stdin);
        assert_eq!(
            unsafe { WaitForSingleObject(child.process.as_raw_handle(), 10_000) },
            WAIT_OBJECT_0
        );
        let mut log = String::new();
        stderr.read_to_string(&mut log).unwrap();
        assert!(child.status().unwrap().unwrap().success(), "{log}");
        assert_eq!(log.trim_end(), "stderr-ok");
        let mut text = String::new();
        stdout.read_to_string(&mut text).unwrap();
        let (argv, input, cwd, utf8, encoding): (Vec<String>, String, PathBuf, String, String) =
            serde_json::from_str(&text).unwrap();
        assert_eq!(argv, args);
        assert_eq!(input, "pipe✓");
        assert_eq!(cwd.canonicalize().unwrap(), dir);
        assert_eq!((utf8.as_str(), encoding.as_str()), ("1", "utf-8"));
        assert_eq!(
            unsafe { WaitForSingleObject(sentinel.as_raw_handle(), 0) },
            WAIT_TIMEOUT,
            "unrelated event was inherited"
        );
    }
}
