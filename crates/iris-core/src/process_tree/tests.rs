use super::*;
fn child(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "process_tree::tests::native_fixture",
        "--ignored",
        "--nocapture",
    ]);
    command.env_clear().env("IRIS_NATIVE_FIXTURE", mode);
    let cwd = std::env::var_os("MPV_LIB_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    command.current_dir(cwd);
    command
}
#[test]
#[ignore = "native subprocess helper, executed by supervisor tests"]
fn native_fixture() {
    match std::env::var("IRIS_NATIVE_FIXTURE").unwrap().as_str() {
        "environment" => {
            assert!(std::env::var_os("USERPROFILE").is_none());
            assert!(std::env::var_os("HOME").is_none());
            assert!(std::env::var_os("PATH").is_none());
            println!("explicit-environment-only");
        }
        "flood" => {
            let data = [b'x'; 16384];
            for _ in 0..1024 {
                if std::io::stdout().write_all(&data).is_err() {
                    return;
                }
            }
        }
        "tree" => {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "process_tree::tests::native_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env("IRIS_NATIVE_FIXTURE", "wait");
            let mut child = command.spawn().unwrap();
            println!("iris-descendant={}", child.id());
            std::io::stdout().flush().unwrap();
            child.wait().unwrap();
        }
        "wait" => std::thread::sleep(Duration::from_secs(30)),
        _ => panic!("invalid native fixture"),
    }
}
#[test]
fn explicit_environment_and_output_limit_are_enforced_in_real_child_processes() {
    let bytes = capture(
        &mut child("environment"),
        &HttpCancel::default(),
        Duration::from_secs(5),
        65536,
    )
    .unwrap();
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .contains("explicit-environment-only")
    );
    let start = Instant::now();
    assert!(matches!(
        capture(
            &mut child("flood"),
            &HttpCancel::default(),
            Duration::from_secs(5),
            1024
        ),
        Err(ErrorCode::QuotaExceeded)
    ));
    assert!(start.elapsed() < Duration::from_secs(3));
}
#[test]
fn cancelling_a_native_task_kills_descendants_and_closes_their_inherited_pipes() {
    let cancel = HttpCancel::default();
    let timer = cancel.clone();
    let started = Instant::now();
    let mut bytes = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            timer.cancel();
        });
        assert!(matches!(
            run(
                &mut child("tree"),
                None,
                &cancel,
                Duration::from_secs(5),
                &mut |part| {
                    bytes.extend_from_slice(part);
                    Ok(())
                }
            ),
            Err(ErrorCode::Cancelled)
        ));
    });
    assert!(started.elapsed() < Duration::from_secs(3));
    let output = String::from_utf8(bytes).unwrap();
    let pid = output
        .lines()
        .find_map(|l| l.strip_prefix("iris-descendant="))
        .unwrap()
        .parse::<u32>()
        .unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if !handle.is_null() {
            let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
            let mut exit = 0;
            assert_ne!(
                unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut exit) },
                0
            );
            assert_ne!(exit, 259, "descendant still running");
        }
    }
    #[cfg(unix)]
    {
        let _ = pid;
    } // Pipe closure and time bound also exercise process-group cleanup.
}
