use super::*;

const TREE_PLUGIN: &str = r#"import json, os, pathlib, subprocess, sys
child = r'''
import os, pathlib, time
with open('child.lock', 'w+b') as f:
    if os.name == 'nt':
        import msvcrt
        msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
    else:
        import fcntl
        fcntl.flock(f, fcntl.LOCK_EX)
    pathlib.Path('ready').touch()
    time.sleep(20)
'''
helper = None
for raw in sys.stdin:
    event = json.loads(raw)['payload']['event']
    if event == 'init':
        helper = subprocess.Popen([sys.executable, '-c', child], stdin=subprocess.DEVNULL)
    elif event == 'shutdown':
        pathlib.Path('shutdown').touch()
# On Unix an abrupt host exit closes stdin, and a cooperative plugin owns helper cleanup.
if not pathlib.Path('shutdown').exists() and os.name != 'nt':
    helper.terminate()
    helper.wait()
pathlib.Path('eof').touch()
"#;

fn install_tree_plugin(env: &TestEnv) -> PathBuf {
    let name = "acme.tree";
    write_ext_plugin(
        &env.ext_dir,
        name,
        &manifest_with(name, Some("main.py"), vec![]),
        TREE_PLUGIN,
    );
    env.ext_dir.join(name)
}

fn wait_for_file(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    path.is_file()
}

fn assert_helper_released_lock(dir: &Path) {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("child.lock"))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while file.try_lock().is_err() {
        assert!(
            Instant::now() < deadline,
            "plugin helper retained its file lock"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn normal_quit_delivers_shutdown_then_eof_and_cleans_up_descendants() {
    assert!(python_available());
    let env = TestEnv::new("normal-quit");
    let dir = install_tree_plugin(&env);
    let mut mgr = env.manager(&[]);
    assert!(wait_for_file(&dir.join("ready")));
    mgr.shutdown();
    assert!(dir.join("shutdown").is_file() && dir.join("eof").is_file());
    assert_helper_released_lock(&dir);
    mgr.shutdown();
}

#[test]
fn abrupt_plugin_host_exit_delivers_unix_eof_or_terminates_windows_job() {
    const FIXTURE: &str = "IRIS_E2E_CHILD_PLUGIN_DIR";
    if let Some(dir) = std::env::var_os(FIXTURE) {
        let _mgr = PluginManager::with_dirs_and_vault(
            Some(Path::new(&dir)),
            None,
            &[],
            CredentialVault::in_memory(),
        );
        thread::sleep(Duration::from_secs(20));
        return;
    }
    assert!(python_available());
    let env = TestEnv::new("abrupt-quit");
    let dir = install_tree_plugin(&env);
    let mut host = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            concat!(
                "process::",
                "abrupt_plugin_host_exit_delivers_unix_eof_or_terminates_windows_job"
            ),
        ])
        .env(FIXTURE, &env.ext_dir)
        .spawn()
        .unwrap();
    let ready = wait_for_file(&dir.join("ready"));
    host.kill().unwrap();
    host.wait().unwrap();
    assert!(ready, "plugin helper did not start");
    #[cfg(unix)]
    assert!(
        wait_for_file(&dir.join("eof")),
        "plugin must receive EOF when the host disappears"
    );
    assert_helper_released_lock(&dir);
}

#[test]
fn exited_plugin_leader_is_reaped_with_its_stdio_inheriting_helper() {
    assert!(python_available());
    let env = TestEnv::new("leader-exit");
    let code = TREE_PLUGIN.replace(
        "    elif event == 'shutdown':",
        "    elif event == 'command':\n        os._exit(37)\n    elif event == 'shutdown':",
    );
    write_ext_plugin(
        &env.ext_dir,
        "tree",
        &manifest_with("acme.tree", Some("main.py"), vec![]),
        &code,
    );
    let mut mgr = env.manager(&[]);
    let dir = env.ext_dir.join("tree");
    assert!(wait_for_file(&dir.join("ready")));
    let _ = mgr.send_to("acme.tree", cmd_ev("quit", PlaybackSnapshot::default()));
    poll_until(&mut mgr, |m, _| {
        m.list().iter().any(|p| {
            p.manifest.id == "acme.tree"
                && matches!(&p.status, PluginStatus::Error(reason) if reason.contains("37"))
        })
    });
    assert!(mgr.list().iter().any(|p| {
        p.manifest.id == "acme.tree"
            && matches!(&p.status, PluginStatus::Error(reason) if reason.contains("37"))
    }));
    assert_helper_released_lock(&dir);
}

#[test]
fn external_command_bursts_deliver_every_response_in_order() {
    assert!(python_available());
    let env = TestEnv::new("burst");
    let code = r#"import json, sys
for raw in sys.stdin:
    ev = json.loads(raw)['payload']['event']
    if ev == 'init':
        for n in range(512):
            print(json.dumps({'type': 'commands', 'commands': [{'command': 'show_toast', 'message': str(n)}]}))
"#;
    write_ext_plugin(
        &env.ext_dir,
        "burst",
        &manifest_with(
            "acme.burst",
            Some("main.py"),
            vec![Permission::Notifications],
        ),
        code,
    );
    let (mut mgr, mut wakes) =
        PluginManager::load_with_dirs_and_vault(Some(&env.ext_dir), None, &[], env.vault.clone());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut values = Vec::new();
    while values.len() < 512 && Instant::now() < deadline {
        if wakes.try_recv().is_ok() {
            values.extend(
                mgr.poll()
                    .commands
                    .into_iter()
                    .filter_map(|(id, cmd)| match cmd {
                        HostCommand::ShowToast { message } if id == "acme.burst" => {
                            Some(message.parse::<usize>().unwrap())
                        }
                        _ => None,
                    }),
            );
        } else {
            thread::sleep(Duration::from_millis(5));
        }
    }
    assert_eq!(values, (0..512).collect::<Vec<_>>());
}

#[test]
fn external_stderr_250_lines_and_large_json_line_never_crash_or_drop() {
    if !python_available() {
        return;
    }
    let env = TestEnv::new("stderr-250");
    let chatty_py = r#"import json, sys
for raw in sys.stdin:
    ev = json.loads(raw).get("payload", {})
    if ev.get("event") == "init":
        for i in range(250):
            sys.stderr.write(f"diag {i} " + "x" * 120 + "\n")
        sys.stderr.flush()
    elif ev.get("event") == "command" and ev.get("id") == "ping":
        for i in range(150):
            sys.stderr.write(f"ping {i}\n")
        sys.stderr.flush()
        entries = [{"id": f"e{i}", "name": f"Item {i}.mp4", "kind": "video", "variants": []} for i in range(1200)]
        page = {"source_id": "s", "folder_id": None, "parent_id": None, "title": "Big", "entries": entries, "next_page": None, "authenticated": True, "auth_hint": None}
        cmds = [{"command": "set_status", "badge": {"id": "ok", "text": "ALIVE-400", "accent": True}}, {"command": "provide_source_page", "request_id": "big", "page": page}]
        cmds.insert(0, {"command": "set_overlay", "nodes": [{"kind": "grid", "rows": 3, "cols": 3}] * 65})
        sys.stdout.write(json.dumps({"type": "commands", "commands": cmds}) + "\n")
        sys.stdout.flush()
    elif ev.get("event") == "shutdown":
        break
"#;
    let chatty_m = manifest_with(
        "acme.chatty",
        Some("main.py"),
        vec![
            Permission::ReadState,
            Permission::UiStatus,
            Permission::UiOverlay,
            Permission::MediaSource,
        ],
    );
    write_ext_plugin(&env.ext_dir, "chatty", &chatty_m, chatty_py);
    write_ext_plugin(
        &env.ext_dir,
        "stall",
        &manifest_with("acme.stall", Some("stall.py"), vec![Permission::ReadState]),
        "import time\ntime.sleep(10)\n",
    );

    let mut mgr = env.manager(&[]);
    let started = Instant::now();
    for _ in 0..500 {
        let _ = mgr.broadcast(&HostEvent::Tick {
            position: 1.0,
            duration: 60.0,
            paused: false,
        });
    }
    assert!(started.elapsed() < Duration::from_millis(250));

    let _ = mgr.send_to("acme.chatty", cmd_ev("ping", PlaybackSnapshot::default()));
    let rep = poll_until(&mut mgr, |m, r| {
        !m.status_badges().is_empty()
            && r.commands.iter().any(|(_, c)| matches!(c, HostCommand::ProvideSourcePage { page, .. } if page.entries.len() == 1200))
    });
    assert!(!rep.commands.is_empty() && mgr.status_badges()[0].1.text == "ALIVE-400");
    assert!(!mgr.overlay_nodes().any(|(id, _)| id == "acme.chatty"));
}
