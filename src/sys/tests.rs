use super::*;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("frust-sys-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(target_os = "linux")]
static SESSION_TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Legacy scenarios run alone so their write permit cannot suppress unrelated cleanup tests.
#[cfg(target_os = "linux")]
fn isolated_session_scenario(name: &str) -> bool {
    let test = format!("sys::tests::{name}");
    if std::env::var("FRUST_SYS_SESSION_SCENARIO").as_deref() == Ok(test.as_str()) {
        return false;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture"])
        .env("FRUST_SYS_SESSION_SCENARIO", &test)
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated session scenario failed: {test}");
            return true;
        }
        if started.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated session scenario exceeded its deadline: {test}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
fn session_paths(tag: &str) -> (PathBuf, Vec<PathBuf>) {
    let dir = scratch(tag);
    let paths: Vec<_> = (0..3)
        .map(|index| dir.join(format!("{index}.lock")))
        .collect();
    for path in &paths {
        fs::write(path, b"live session").unwrap();
    }
    (dir, paths)
}

#[cfg(target_os = "linux")]
fn forced_legacy(paths: &[PathBuf]) -> io::Result<SessionLocks> {
    try_session_locks_with(paths, &mut imp::open_session_lock, &mut |file, mode| {
        imp::try_lock_session_with(file, mode, &mut |file, mode, kind| {
            if mode == SessionRecordMode::Ofd {
                Err(io::Error::from_raw_os_error(libc::EINVAL))
            } else {
                imp::session_record_lock(file, mode, kind)
            }
        })
    })
}

// A separate process probes native locks without closing an alias in the holder process.
#[cfg(target_os = "linux")]
fn assert_session_native_exclusion(path: &Path) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sys::tests::session_native_exclusion_child"])
        .env("FRUST_SYS_SESSION_PROBE", path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("native session probe exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn session_native_exclusion_child() {
    let Some(path) = std::env::var_os("FRUST_SYS_SESSION_PROBE") else {
        return;
    };
    let file = imp::open_session_lock(Path::new(&path)).unwrap();
    assert!(file.try_lock().is_err());
    assert!(file.try_lock_shared().is_err());
    for kind in [libc::F_RDLCK, libc::F_WRLCK] {
        let error =
            imp::session_record_lock(&file, SessionRecordMode::Posix, kind as _).unwrap_err();
        assert!(matches!(
            error.raw_os_error(),
            Some(libc::EAGAIN | libc::EACCES)
        ));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn session_ofd_alias_close_and_explicit_unlock_with_clones() {
    let _serial = SESSION_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (dir, paths) = session_paths("ofd-alias");
    let guard = try_session_locks(&paths[..1]).unwrap();
    assert_eq!(guard.record_mode, SessionRecordMode::Ofd);
    drop(imp::open_session_lock(&paths[0]).unwrap());
    assert_session_native_exclusion(&paths[0]);
    let clone = guard.files[0].1.try_clone().unwrap();
    drop(guard);
    let next = try_session_locks(&paths[..1]).unwrap();
    assert!(next.revalidate());
    drop(next);
    drop(clone);
    fs::remove_dir_all(dir).unwrap();
}

// Every failed-attempt descriptor must close before any legacy replacement is opened.
#[cfg(target_os = "linux")]
#[test]
fn session_fallback_closes_all_ofd_descriptors_before_reopening() {
    if isolated_session_scenario("session_fallback_closes_all_ofd_descriptors_before_reopening") {
        return;
    }
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;
    let _serial = SESSION_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (dir, paths) = session_paths("ofd-restart");
    let descriptors = RefCell::new(Vec::<(std::os::fd::RawFd, PathBuf)>::new());
    let opens = std::cell::Cell::new(0);
    let mut records = 0;
    let guard = try_session_locks_with(
        &paths,
        &mut |path| {
            let count = opens.get();
            if count == paths.len() {
                for (fd, original) in descriptors.borrow().iter() {
                    let linked = fs::read_link(format!("/proc/self/fd/{fd}")).ok();
                    assert_ne!(linked.as_deref(), Some(original.as_path()));
                }
                assert!(matches!(
                    SESSION_LOCK_SERIAL.try_read(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
                let probe = imp::open_session_lock(&paths[0])?;
                imp::session_record_lock(&probe, SessionRecordMode::Ofd, libc::F_WRLCK as _)?;
                imp::session_record_lock(&probe, SessionRecordMode::Ofd, libc::F_UNLCK as _)?;
                probe.try_lock().map_err(io::Error::from)?;
                probe.unlock()?;
            }
            let file = imp::open_session_lock(path)?;
            if count < paths.len() {
                descriptors
                    .borrow_mut()
                    .push((file.as_raw_fd(), path.to_path_buf()));
            }
            opens.set(count + 1);
            Ok(file)
        },
        &mut |file, mode| {
            imp::try_lock_session_with(file, mode, &mut |file, mode, kind| {
                records += 1;
                if records == 2 {
                    Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))
                } else {
                    imp::session_record_lock(file, mode, kind)
                }
            })
        },
    )
    .unwrap();
    assert_eq!(opens.get(), 2 * paths.len());
    assert_eq!(guard.record_mode, SessionRecordMode::Posix);
    for path in &paths {
        assert_session_native_exclusion(path);
    }
    drop(guard);
    let retry = try_session_locks(&paths).unwrap();
    drop(retry);
    fs::remove_dir_all(dir).unwrap();
}

// A live OFD reader must prevent the POSIX restart from reopening paths.
#[cfg(target_os = "linux")]
#[test]
fn session_live_ofd_reader_refuses_fallback_before_any_reopen() {
    let _serial = SESSION_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (dir, paths) = session_paths("ofd-reader");
    let first = try_session_locks(&paths[..1]).unwrap();
    let mut opens = 0;
    let mut modes = Vec::new();
    let error = try_session_locks_with(
        &paths[1..],
        &mut |path| {
            opens += 1;
            imp::open_session_lock(path)
        },
        &mut |file, mode| {
            modes.push(mode);
            imp::try_lock_session_with(file, mode, &mut |_, _, _| {
                Err(io::Error::from_raw_os_error(libc::ENOSYS))
            })
        },
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(opens, 2);
    assert_eq!(modes, [SessionRecordMode::Ofd]);
    assert_session_native_exclusion(&paths[0]);
    drop(first);
    for path in &paths {
        assert_eq!(fs::read(path).unwrap(), b"live session");
    }
    fs::remove_dir_all(dir).unwrap();
}

// Legacy serialization must reject aliases before open can release process-owned locks.
#[cfg(target_os = "linux")]
#[test]
fn session_legacy_write_refuses_every_alias_open_and_releases_after_drop() {
    if isolated_session_scenario(
        "session_legacy_write_refuses_every_alias_open_and_releases_after_drop",
    ) {
        return;
    }
    let _serial = SESSION_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (dir, paths) = session_paths("posix-permit");
    let first = forced_legacy(&paths[..1]).unwrap();
    assert_eq!(first.record_mode, SessionRecordMode::Posix);
    for batch in [
        vec![paths[1].clone()],
        vec![paths[1].clone(), paths[0].clone()],
    ] {
        let error = try_session_locks_with(
            &batch,
            &mut |_| panic!("legacy guard must refuse before opening any alias"),
            &mut imp::try_lock_session,
        )
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_session_native_exclusion(&paths[0]);
    }
    let clone = first.files[0].1.try_clone().unwrap();
    drop(first);
    let next = try_session_locks(&paths).unwrap();
    assert!(next.revalidate());
    drop(next);
    drop(clone);
    for path in &paths {
        assert_eq!(fs::read(path).unwrap(), b"live session");
    }
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn session_only_record_unsupported_errors_restart_and_identity_changes_fail_closed() {
    if isolated_session_scenario(
        "session_only_record_unsupported_errors_restart_and_identity_changes_fail_closed",
    ) {
        return;
    }
    let _serial = SESSION_TEST_SERIAL
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (dir, paths) = session_paths("ofd-errors");
    for code in [
        libc::EINVAL,
        libc::EOPNOTSUPP,
        libc::ENOSYS,
        libc::EACCES,
        libc::EAGAIN,
        libc::ENOLCK,
        libc::EIO,
    ] {
        let mut opens = 0;
        let mut modes = Vec::new();
        let result = try_session_locks_with(
            &paths[..1],
            &mut |path| {
                opens += 1;
                imp::open_session_lock(path)
            },
            &mut |file, mode| {
                modes.push(mode);
                imp::try_lock_session_with(file, mode, &mut |file, mode, kind| {
                    if mode == SessionRecordMode::Ofd {
                        Err(io::Error::from_raw_os_error(code))
                    } else {
                        imp::session_record_lock(file, mode, kind)
                    }
                })
            },
        );
        let unsupported = [libc::EINVAL, libc::EOPNOTSUPP, libc::ENOSYS].contains(&code);
        assert_eq!(result.is_ok(), unsupported);
        assert_eq!(opens, if unsupported { 2 } else { 1 });
        assert_eq!(modes.len(), opens);
        drop(result);
        assert_eq!(fs::read(&paths[0]).unwrap(), b"live session");
    }
    let mut opens = 0;
    let error = try_session_locks_with(
        &paths[..1],
        &mut |_| {
            opens += 1;
            Err(io::Error::from(io::ErrorKind::Unsupported))
        },
        &mut |_, _| panic!("an open error must stop acquisition"),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(opens, 1);
    let busy = imp::open_session_lock(&paths[0]).unwrap();
    busy.try_lock().unwrap();
    let mut busy_opens = 0;
    let error = try_session_locks_with(
        &paths[..1],
        &mut |path| {
            busy_opens += 1;
            imp::open_session_lock(path)
        },
        &mut |file, mode| {
            imp::try_lock_session_with(file, mode, &mut |_, _, _| {
                panic!("flock contention must stop before the record syscall")
            })
        },
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(busy_opens, 1);
    drop(busy);
    let moved = dir.join("moved.lock");
    let error = try_session_locks_with(
        &paths[..1],
        &mut |path| {
            let file = imp::open_session_lock(path)?;
            fs::rename(path, &moved)?;
            fs::write(path, b"replacement session")?;
            Ok(file)
        },
        &mut imp::try_lock_session,
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(fs::read(&moved).unwrap(), b"live session");
    assert_eq!(fs::read(&paths[0]).unwrap(), b"replacement session");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn directory_watcher_falls_back_to_a_bounded_wait() {
    let mut watcher = DirectoryWatcher { inner: None };
    let timeout = Duration::from_millis(10);
    let started = Instant::now();
    assert!(!watcher.wait(timeout));
    assert!(started.elapsed() >= timeout);
}

#[test]
fn directory_watcher_unavailable_path_uses_fallback() {
    let dir = scratch("watch-unavailable");
    let mut watcher = DirectoryWatcher::new(&[&dir.join("absent")]);
    assert!(watcher.inner.is_none());
    assert!(!watcher.wait(Duration::ZERO));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn meta_reports_identity_links_and_times() {
    let dir = scratch("meta");
    let file = dir.join("f");
    fs::write(&file, b"hello").unwrap();
    let meta = stat(&file).unwrap();
    assert_eq!(meta.len(), 5);
    assert_eq!(meta.nlink(), 1);
    assert!(meta.ino() != 0);
    let (secs, nsec) = meta.mtime();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((now - secs).abs() < 600);
    assert!((0..1_000_000_000).contains(&nsec));
    let open = File::open(&file).unwrap();
    let by_handle = fstat(&open).unwrap();
    assert_eq!((by_handle.dev(), by_handle.ino()), (meta.dev(), meta.ino()));
    fs::hard_link(&file, dir.join("g")).unwrap();
    assert_eq!(stat(&file).unwrap().nlink(), 2);
    // Windows identity requires stat, lstat or fstat rather than bare std metadata.
    if cfg!(unix) {
        assert_eq!(Meta::from(fs::metadata(&file).unwrap()).ino(), meta.ino());
    }
    assert!(stat(&dir.join("absent")).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn positional_io_leaves_the_cursor_alone() {
    let dir = scratch("positional");
    let path = dir.join("f");
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    write_all_at(&file, b"world", 6).unwrap();
    write_all_at(&file, b"hello ", 0).unwrap();
    let mut buf = [0u8; 5];
    read_exact_at(&file, &mut buf, 6).unwrap();
    assert_eq!(&buf, b"world");
    assert_eq!(fs::read(&path).unwrap(), b"hello world");
    assert!(
        read_exact_at(&file, &mut [0u8; 8], 8).is_err(),
        "reading past the end fails"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn locks_conflict_between_handles() {
    let dir = scratch("lock");
    let path = dir.join("f");
    fs::write(&path, b"").unwrap();
    let (a, b) = (File::open(&path).unwrap(), File::open(&path).unwrap());
    lock(&a, LockKind::Exclusive).unwrap();
    assert!(matches!(
        try_lock(&b, LockKind::Exclusive),
        Err(fs::TryLockError::WouldBlock)
    ));
    assert!(matches!(
        try_lock(&b, LockKind::Shared),
        Err(fs::TryLockError::WouldBlock)
    ));
    unlock(&a).unwrap();
    try_lock(&b, LockKind::Shared).unwrap();
    try_lock(&a, LockKind::Shared).unwrap();
    assert!(matches!(
        try_lock(&File::open(&path).unwrap(), LockKind::Exclusive),
        Err(fs::TryLockError::WouldBlock)
    ));
    unlock(&a).unwrap();
    unlock(&b).unwrap();
    lock(&b, LockKind::Shared).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rename_replaces_an_existing_target() {
    let dir = scratch("rename");
    fs::write(dir.join("from"), b"new").unwrap();
    fs::write(dir.join("to"), b"old").unwrap();
    rename(&dir.join("from"), &dir.join("to")).unwrap();
    assert_eq!(fs::read(dir.join("to")).unwrap(), b"new");
    assert!(!dir.join("from").exists());
    assert!(rename(&dir.join("from"), &dir.join("other")).is_err());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn set_mtime_now_moves_files_and_directories_forward() {
    let dir = scratch("mtime");
    let file = dir.join("f");
    fs::write(&file, b"x").unwrap();
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let old = SystemTime::now() - Duration::from_secs(7200);
    File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old)
        .unwrap();
    // Opening directories on Windows requires backup semantics, so only Unix ages this one.
    #[cfg(unix)]
    File::open(&sub).unwrap().set_modified(old).unwrap();
    for path in [&file, &sub] {
        set_mtime_now(path).unwrap();
        let age = fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap_or_default();
        assert!(age < Duration::from_secs(600), "{}", path.display());
    }
    assert!(set_mtime_now(&dir.join("absent")).is_err());
    assert!(
        !dir.join("absent").exists(),
        "touching a missing path creates nothing"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pipe_reads_never_block_and_report_end_of_stream() {
    let (reader, mut writer) = io::pipe().unwrap();
    let mut pipe = Pipe::from(reader);
    let mut buf = [0u8; 16];
    assert_eq!(
        pipe.read(&mut buf).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        wait_readable([Some(&pipe), None], Duration::from_millis(10)),
        [false, false]
    );
    writer.write_all(b"hi").unwrap();
    assert_eq!(
        wait_readable([None, Some(&pipe)], Duration::from_secs(5)),
        [false, true]
    );
    assert_eq!(pipe.read(&mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"hi");
    drop(writer);
    assert_eq!(
        wait_readable([Some(&pipe), None], Duration::from_secs(5)),
        [true, false]
    );
    assert_eq!(pipe.read(&mut buf).unwrap(), 0);
}
