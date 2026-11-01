//! A frontend lease admits a bounded, process-local queue of immutable stores.

use std::collections::{HashSet, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::paths::{DEFERRED_SLOT_PREFIX, Dirs};
use crate::sys::store_transport::{self, Connection, ControlReader, ControlWriter, Listener};
use crate::sys::{self, LockKind};
use crate::{fsx, lock, wrapper};

pub(crate) const CAPABILITY_ENV: &str = "FRUST_INTERNAL_STORE_BROKER";
const BROKER_ARG: &str = "--store-broker";
const VERSION: u32 = 1;
const QUEUE_LIMIT: usize = 8;
const MAX_JOB: usize = 2 * 1024 * 1024;
const MAX_REQUEST: usize = 16 * 1024;
const HANDSHAKE: Duration = Duration::from_millis(100);
const CLIENT_WAIT: Duration = Duration::from_secs(1);
const RUN_MAX_AGE: Duration = Duration::from_secs(3600);
const ACCEPTED: &[u8] = b"frust-store-v1:accepted";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capability {
    version: u32,
    home: PathBuf,
    run: PathBuf,
    token: String,
}

impl Capability {
    // Derive the short endpoint only from the authenticated token, independent of home-path length.
    fn endpoint(&self) -> PathBuf { endpoint_base().join(format!("b-{}", self.token)) }
    fn socket(&self) -> PathBuf { self.endpoint().join("s") }

    fn verify_shape(&self, dirs: &Dirs) -> io::Result<()> {
        if self.version != VERSION || self.home != absolute_home(dirs)?
            || !self.home.is_absolute() || !self.run.is_absolute()
            || self.run.parent() != Some(self.home.join("tmp").as_path())
            || self.token.len() != 64 || !self.token.bytes().all(|c| c.is_ascii_hexdigit())
        { return Err(invalid("invalid store broker capability")); }
        let Some(pid) = self.run.file_name().and_then(|s| s.to_str()).and_then(|s| s.strip_prefix("store-broker-")).and_then(|s| s.strip_suffix(&format!("-{}", &self.token[..16]))) else {
            return Err(invalid("invalid store broker run name"));
        };
        if pid.is_empty() || !pid.bytes().all(|c| c.is_ascii_digit()) { return Err(invalid("invalid store broker run pid")) }
        Ok(())
    }

    fn verify(&self, dirs: &Dirs) -> io::Result<()> {
        self.verify_shape(dirs)?;
        sys::verify_private_dir(&self.run).map_err(io::Error::other)?;
        let (bytes, _) = read_regular(&self.run.join("capability.json"), MAX_REQUEST, true)?;
        let saved: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if saved.version != self.version || saved.home != self.home || saved.run != self.run || saved.token != self.token {
            return Err(invalid("store broker capability changed"));
        }
        verify_endpoint(self)?;
        Ok(())
    }
}

pub(crate) struct BrokerLaunch {
    pending: Option<Capability>,
    lease: Option<StoreBrokerLease>,
    demands: u8,
}

impl BrokerLaunch {
    pub(crate) fn prepare(dirs: &Dirs, cmd: &mut Command) -> io::Result<Self> {
        let home = absolute_home(dirs)?;
        let token = store_transport::random_capability()?;
        let run = home.join("tmp").join(format!("store-broker-{}-{}", std::process::id(), &token[..16]));
        let cap = Capability { version: VERSION, home, run, token };
        cmd.env(CAPABILITY_ENV, serde_json::to_string(&cap).map_err(io::Error::other)?);
        Ok(Self { pending: Some(cap), lease: None, demands: 0 })
    }

    pub(crate) fn note_store_demands(&mut self, count: u8) {
        // Count actual compile attempts, not distinct keys; one attempt keeps the legacy helper.
        self.demands = self.demands.saturating_add(count).min(2);
        if self.demands == 2 {
            self.activate();
        }
    }

    // A failed attempt stays disabled; producers retain their existing helper or inline fallback.
    fn activate(&mut self) {
        if let Some(cap) = self.pending.take() {
            self.lease = StoreBrokerLease::materialize(cap).ok();
        }
    }

    pub(crate) fn finish(mut self) {
        self.pending.take();
        if let Some(lease) = self.lease.take() { lease.finish(); }
    }
}

pub(crate) struct StoreBrokerLease {
    control: Option<ControlWriter>,
    bootstrap: Option<JoinHandle<()>>,
    lease: Option<File>,
}

impl StoreBrokerLease {
    fn materialize(cap: Capability) -> io::Result<Self> {
        let dirs = Dirs::at(cap.home.clone());
        lock::ensure_dir(&dirs.tmp(), None)?;
        sys::create_dir_with_mode(&cap.run, 0o700)?;
        let mut cleanup = RunDirectory::new(cap.run.clone())?;
        sys::verify_private_dir(&cap.run).map_err(io::Error::other)?;
        let mut lease_options = File::options();
        lease_options.read(true).write(true).create_new(true);
        sys::with_mode(&mut lease_options, 0o600);
        let lease = lease_options.open(cap.run.join("lease"))?;
        sys::try_lock(&lease, LockKind::Shared).map_err(io::Error::from)?;
        check_regular_identity(&cap.run.join("lease"), &lease, true)?;
        // Keep one shared description in the frontend after the bootstrap hands cleanup to the broker.
        cleanup.lease = Some(lease.try_clone()?);
        let mut options = File::options();
        options.write(true).create_new(true);
        sys::with_mode(&mut options, 0o600);
        options.open(cap.run.join("capability.tmp"))?.write_all(&serde_json::to_vec(&cap).map_err(io::Error::other)?)?;
        sys::rename(&cap.run.join("capability.tmp"), &cap.run.join("capability.json"))?;
        crate::paths::private_temp_dir(Path::new("/tmp")).map_err(io::Error::other)?;
        sys::create_dir_with_mode(&cap.endpoint(), 0o700)?;
        cleanup.endpoint = Some(DirectoryIdentity::capture(cap.endpoint())?);
        verify_endpoint(&cap)?;
        let listener = Listener::bind_private(&cap.socket())?;
        let (reader, writer) = store_transport::control_pair()?;
        let bootstrap_cap = serde_json::to_string(&cap).map_err(io::Error::other)?;
        let bootstrap = thread::Builder::new().name("frust-store-bootstrap".into()).spawn(move || {
            let first = loop {
                match store_transport::accept_or_closed(&listener, &reader, Instant::now() + CLIENT_WAIT) {
                    Ok(Some(first)) => break Some(first),
                    Ok(None) if !reader.is_closed().unwrap_or(true) => continue,
                    _ => break None,
                }
            };
            if let Some(first) = first
                && !reader.is_closed().unwrap_or(true)
                && spawn_broker(&listener, &first, &reader, &bootstrap_cap).is_ok()
            {
                // Ownership passes to the process; finishing the frontend never joins its drain.
                cleanup.disarm();
            }
        })?;
        Ok(Self { control: Some(writer), bootstrap: Some(bootstrap), lease: Some(lease) })
    }

    pub(crate) fn finish(mut self) { self.close(); }

    fn close(&mut self) {
        if let Some(writer) = self.control.take() { writer.close(); }
        if let Some(bootstrap) = self.bootstrap.take() { let _ = bootstrap.join(); }
        self.lease.take();
    }
}

impl Drop for StoreBrokerLease {
    fn drop(&mut self) { self.close(); }
}

fn spawn_broker(listener: &Listener, first: &Connection, reader: &ControlReader, cap: &str) -> io::Result<()> {
    let current = env::current_exe()?;
    let name = format!("frust-wrap{}", env::consts::EXE_SUFFIX);
    let sibling = current.parent().ok_or_else(|| invalid("no executable directory"))?.join(&name);
    let exe = if current.file_name().is_some_and(|n| n == name.as_str()) { current } else { sibling };
    let mut cmd = Command::new(exe);
    cmd.arg(BROKER_ARG).arg(listener.descriptor_tag()).arg(first.descriptor_tag()).arg(reader.descriptor_tag());
    cmd.env(CAPABILITY_ENV, cap).env_remove("CARGO_MAKEFLAGS").env_remove("MAKEFLAGS").env_remove("MFLAGS");
    store_transport::detach_broker_command(&mut cmd, &[listener.as_file(), first.as_file(), reader.as_file()])?;
    cmd.spawn().map(drop)
}

pub(crate) struct AdmissionPermit {
    cap: Capability,
    reservation: File,
}

impl AdmissionPermit {
    pub(crate) fn try_reserve(dirs: &Dirs) -> io::Result<Option<Self>> {
        let Some(encoded) = env::var_os(CAPABILITY_ENV) else { return Ok(None) };
        let Some(encoded) = encoded.to_str().filter(|s| s.len() <= MAX_REQUEST) else { return Ok(None) };
        let Ok(cap) = serde_json::from_str::<Capability>(encoded) else { return Ok(None) };
        Self::reserve_cap(dirs, cap)
    }

    fn reserve_cap(dirs: &Dirs, cap: Capability) -> io::Result<Option<Self>> {
        if cap.verify(dirs).is_err() { return Ok(None) }
        let dirs = Dirs::at(cap.home.clone());
        let Some(reservation) = reserve_global(&dirs)? else { return Ok(None) };
        Ok(Some(Self { cap, reservation }))
    }

    pub(crate) fn submit(self, producer_job: &Path, key: Option<&File>) -> io::Result<bool> {
        let result: io::Result<bool> = (|| {
            let deadline = Instant::now() + CLIENT_WAIT;
            let connection = Connection::connect_verified(&self.cap.socket(), deadline)?;
            let request = Request { version: VERSION, token: self.cap.token.clone(), home: self.cap.home.clone(), job: producer_job.to_owned() };
            let bytes = serde_json::to_vec(&request).map_err(io::Error::other)?;
            if bytes.len() > MAX_REQUEST { return Ok(false) }
            let mut files = vec![&self.reservation];
            files.extend(key);
            connection.send(&bytes, &files, deadline)?;
            let (ack, files) = connection.receive(64, deadline)?;
            Ok(ack == ACCEPTED && files.is_empty())
        })();
        // A missing ACK is never retried: the independent broker marker may already own the job.
        Ok(result.unwrap_or(false))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    token: String,
    home: PathBuf,
    job: PathBuf,
}

fn reservation_path(dirs: &Dirs, index: usize) -> PathBuf { dirs.locks().join(format!("store-queue-{index}")) }

fn reserve_global(dirs: &Dirs) -> io::Result<Option<File>> {
    // Stable files coordinate live reservations across every frontend sharing this home.
    lock::ensure_dir(&dirs.locks(), lock::shared_mode(dirs))?;
    for index in 0..QUEUE_LIMIT {
        let path = reservation_path(dirs, index);
        let file = lock::open_lock_file(&path, lock::shared_mode(dirs))?;
        check_regular_identity(&path, &file, true)?;
        if try_exclusive(&file)? { return Ok(Some(file)) }
    }
    Ok(None)
}

fn validate_reservation(dirs: &Dirs, reservation: &File) -> io::Result<usize> {
    let received = sys::fstat(reservation)?;
    let index = (0..QUEUE_LIMIT).find(|&i| {
        let path = reservation_path(dirs, i);
        sys::lstat(&path).is_ok_and(|m| m.is_file() && m.nlink() == 1 && same_inode(&m, &received))
    });
    if index.is_none() || !received.is_file() || received.nlink() != 1 || !try_exclusive(reservation)? {
        return Err(invalid("invalid store queue reservation"));
    }
    Ok(index.unwrap())
}

pub(crate) fn broker_entry(args: &[OsString]) -> Option<i32> {
    if args.first().is_none_or(|a| a != BROKER_ARG) { return None }
    Some(broker_from_args(args).map_or(1, |_| 0))
}

fn broker_from_args(args: &[OsString]) -> io::Result<()> {
    let [_, listener, first, reader] = args else { return Err(invalid("invalid store broker invocation")) };
    let tags = [listener, first, reader].map(|s| s.to_str().ok_or_else(|| invalid("invalid broker descriptor")));
    let [listener, first, reader] = tags;
    let (listener, first, reader) = (listener?, first?, reader?);
    if listener == first || listener == reader || first == reader { return Err(invalid("duplicate broker descriptor")) }
    let encoded = env::var(CAPABILITY_ENV).map_err(io::Error::other)?;
    if encoded.len() > MAX_REQUEST { return Err(invalid("oversize broker capability")) }
    let cap: Capability = serde_json::from_str(&encoded).map_err(io::Error::other)?;
    let dirs = Dirs::at(cap.home.clone());
    cap.verify_shape(&dirs)?;
    sys::verify_private_dir(&cap.run).map_err(io::Error::other)?;
    // An ACK cannot precede this independent shared lease, even after the frontend has exited.
    let Some(lease) = open_run_lease(&cap.run, LockKind::Shared)? else { return Err(invalid("broker run is being retired")) };
    cap.verify(&dirs)?;
    // SAFETY: unique tags are the three owned descriptors whitelisted by spawn_broker.
    let listener = unsafe { Listener::from_inherited(listener)? };
    // SAFETY: this distinct tag transfers the initial accepted connection exactly once.
    let first = unsafe { Connection::from_inherited(first)? };
    // SAFETY: this distinct tag transfers the frontend control reader exactly once.
    let reader = unsafe { ControlReader::from_inherited(reader)? };
    let mut cleanup = RunDirectory::new(cap.run.clone())?;
    cleanup.endpoint = Some(DirectoryIdentity::capture(cap.endpoint())?);
    cleanup.lease = Some(lease);
    let mut config = Config::from_env();
    config.debug = false;
    serve(&dirs, &cap, &listener, first, &reader, &config)
}

struct OwnedMarker {
    _lease: File,
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for OwnedMarker {
    fn drop(&mut self) {
        if sys::lstat(&self.path).is_ok_and(|m| m.is_file() && m.dev() == self.dev && m.ino() == self.ino) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct QueuedJob {
    marker: OwnedMarker,
    job: wrapper::BrokerJob,
    _key: Option<File>,
    _reservation: File,
    reservation_index: usize,
}

#[derive(Default)]
struct QueueState {
    jobs: VecDeque<QueuedJob>,
    reservations: HashSet<usize>,
    closed: bool,
    #[cfg(test)]
    started_workers: usize,
}
type SharedQueue = Arc<(Mutex<QueueState>, Condvar)>;

fn serve(dirs: &Dirs, cap: &Capability, listener: &Listener, first: Connection, control: &ControlReader, config: &Config) -> io::Result<()> {
    serve_with_worker_start(dirs, cap, listener, first, control, config, (slot_count(), |_, _| Ok(())))
}

fn serve_with_worker_start(
    dirs: &Dirs, cap: &Capability, listener: &Listener, first: Connection, control: &ControlReader, config: &Config,
    startup: (usize, impl FnMut(usize, &SharedQueue) -> io::Result<()>),
) -> io::Result<()> {
    let queue: SharedQueue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
    thread::scope(|scope| {
        let result = (|| {
            let (count, mut before_start) = startup;
            for index in 0..count {
                before_start(index, &queue)?;
                let queue = Arc::clone(&queue);
                thread::Builder::new().spawn_scoped(scope, move || {
                    // Workers already own their threads, so background priority ends with this loop.
                    sys::lower_thread_priority();
                    worker(dirs, config, &queue);
                })?;
            }
            let mut next = Some(first);
            loop {
                if control.is_closed()? { break }
                if let Some(connection) = next.take() {
                    let _ = admit(dirs, cap, &connection, control, &queue);
                }
                next = store_transport::accept_or_closed(listener, control, Instant::now() + HANDSHAKE)?;
            }
            Ok(())
        })();
        // Close before scoped joins even when a later worker could not start.
        let (state, wake) = &*queue;
        state.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        wake.notify_all();
        result
    })
}

fn admit(dirs: &Dirs, cap: &Capability, connection: &Connection, control: &ControlReader, queue: &SharedQueue) -> io::Result<()> {
    let deadline = Instant::now() + HANDSHAKE;
    let (bytes, mut files) = connection.receive(MAX_REQUEST, deadline)?;
    let request: Request = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if request.version != VERSION || request.token != cap.token || request.home != cap.home || control.is_closed()? {
        return Err(invalid("unauthenticated store request"));
    }
    let path = request.job;
    if !(1..=2).contains(&files.len()) { return Err(invalid("store descriptor count")) }
    let reservation = files.remove(0);
    let key = files.pop();
    let reservation_index = validate_reservation(dirs, &reservation)?;
    let (bytes, marker) = own_job_marker(dirs, &path)?;
    let job = wrapper::BrokerJob::decode(&bytes, dirs, key.as_ref())?;
    if control.is_closed()? { return Err(invalid("frontend lease closed")) }
    let (state, wake) = &**queue;
    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
    if state.closed || state.reservations.len() >= QUEUE_LIMIT || !state.reservations.insert(reservation_index) {
        return Err(invalid("store queue closed, full, or reservation reused"));
    }
    state.jobs.push_back(QueuedJob { marker, job, _key: key, _reservation: reservation, reservation_index });
    wake.notify_one();
    drop(state);
    connection.send(ACCEPTED, &[], deadline)
}

fn own_job_marker(dirs: &Dirs, path: &Path) -> io::Result<(Vec<u8>, OwnedMarker)> {
    if !path.is_absolute() || path.parent() != Some(dirs.deferred().as_path()) || !valid_job_name(path) {
        return Err(invalid("invalid producer job path"));
    }
    if !sys::owned_by_me(path) {
        return Err(invalid("producer job belongs to another user"));
    }
    let lease = wrapper::lease_deferred_job(path)?;
    let (bytes, identity) = read_regular(path, MAX_JOB, true)?;
    if !same_inode(&sys::fstat(&lease)?, &identity) {
        return Err(invalid("job lease changed"));
    }
    let name = format!(
        "job-{}-broker-{}.json",
        std::process::id(),
        store_transport::random_capability()?
    );
    let owned = dirs.deferred().join(name);
    // Linking precedes queue admission and ACK, so producer cleanup cannot erase accepted work.
    fs::hard_link(path, &owned)?;
    let linked = sys::lstat(&owned)?;
    let marker = OwnedMarker {
        _lease: lease,
        path: owned,
        dev: linked.dev(),
        ino: linked.ino(),
    };
    if !linked.is_file()
        || !same_inode(&linked, &identity)
        || linked.len() != identity.len()
        || linked.mtime() != identity.mtime()
        || !(1..=2).contains(&linked.nlink())
    {
        return Err(invalid("producer job changed while linking"));
    }
    Ok((bytes, marker))
}

fn worker(dirs: &Dirs, config: &Config, queue: &SharedQueue) {
    #[cfg(test)]
    {
        let (state, wake) = &**queue;
        state.lock().unwrap_or_else(|p| p.into_inner()).started_workers += 1;
        wake.notify_all();
    }
    loop {
        let queued = {
            let (state, wake) = &**queue;
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            while state.jobs.is_empty() && !state.closed { state = wake.wait(state).unwrap_or_else(|p| p.into_inner()); }
            let Some(job) = state.jobs.pop_front() else { return };
            job
        };
        let reservation_index = queued.reservation_index;
        let slot = wait_execution_slot(dirs, None);
        if let Ok(Some(slot)) = slot {
            let QueuedJob { marker, job, _key, _reservation, .. } = queued;
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.execute(config)));
            drop(marker);
            drop(slot);
            drop(_key);
            drop(_reservation);
        } else {
            drop(queued);
        }
        queue.0.lock().unwrap_or_else(|p| p.into_inner()).reservations.remove(&reservation_index);
    }
}

fn slot_count() -> usize { (thread::available_parallelism().map_or(2, |n| n.get()) / 2).clamp(1, 4) }

fn wait_execution_slot(dirs: &Dirs, deadline: Option<Instant>) -> io::Result<Option<File>> {
    lock::ensure_dir(&dirs.deferred(), None)?;
    loop {
        for i in 0..slot_count() {
            let path = dirs.deferred().join(format!("{DEFERRED_SLOT_PREFIX}{i}"));
            let file = lock::open_lock_file(&path, None)?;
            check_regular_identity(&path, &file, true)?;
            if try_exclusive(&file)? { return Ok(Some(file)) }
        }
        // Accepted work has no production expiry; finite deadlines serve contention tests.
        let pause = if let Some(deadline) = deadline {
            if Instant::now() >= deadline { return Ok(None) }
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now()))
        } else {
            Duration::from_millis(10)
        };
        thread::sleep(pause);
    }
}

fn try_exclusive(file: &File) -> io::Result<bool> {
    match sys::try_lock(file, LockKind::Exclusive) {
        Ok(()) => Ok(true),
        Err(fs::TryLockError::WouldBlock) => Ok(false),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn read_regular(path: &Path, cap: usize, single_link: bool) -> io::Result<(Vec<u8>, sys::Meta)> {
    // Check size before allocating and again after the capped read of the same inode.
    let before = sys::lstat(path)?;
    if !before.is_file() || before.len() > cap as u64 || (single_link && before.nlink() != 1) {
        return Err(invalid("invalid or oversized store metadata"));
    }
    let file = File::open(path)?;
    let opened = check_regular_identity(path, &file, single_link)?;
    if !same_snapshot(&before, &opened) { return Err(invalid("metadata changed before read")) }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    (&file).take(cap as u64 + 1).read_to_end(&mut bytes)?;
    let after = check_regular_identity(path, &file, single_link)?;
    if bytes.len() > cap || bytes.len() as u64 != after.len() || !same_snapshot(&opened, &after) {
        return Err(invalid("metadata changed during read"));
    }
    Ok((bytes, after))
}

fn check_regular_identity(path: &Path, file: &File, single_link: bool) -> io::Result<sys::Meta> {
    let path_meta = sys::lstat(path)?;
    let file_meta = sys::fstat(file)?;
    if !path_meta.is_file() || !file_meta.is_file() || !same_inode(&path_meta, &file_meta)
        || (single_link && (path_meta.nlink() != 1 || file_meta.nlink() != 1))
    { return Err(invalid("store file identity mismatch")) }
    Ok(file_meta)
}

fn same_inode(a: &sys::Meta, b: &sys::Meta) -> bool { a.dev() == b.dev() && a.ino() == b.ino() }
fn same_snapshot(a: &sys::Meta, b: &sys::Meta) -> bool {
    same_inode(a, b) && a.len() == b.len() && a.mtime() == b.mtime() && a.ctime() == b.ctime()
}
fn absolute_home(dirs: &Dirs) -> io::Result<PathBuf> {
    Ok(env::current_dir()?.join(dirs.root()))
}
fn valid_job_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("job-")).and_then(|n| n.strip_suffix(".json")) else { return false };
    let Some((pid, unique)) = name.split_once('-') else { return false };
    !pid.is_empty() && pid.bytes().all(|c| c.is_ascii_digit()) && !unique.is_empty()
        && unique.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}
fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn endpoint_base() -> PathBuf { Path::new("/tmp").join(format!("frust-{}", sys::user_id())) }

fn verify_endpoint(cap: &Capability) -> io::Result<()> {
    sys::verify_private_dir(&endpoint_base()).map_err(io::Error::other)?;
    sys::verify_private_dir(&cap.endpoint()).map_err(io::Error::other)
}

fn open_run_lease(run: &Path, kind: LockKind) -> io::Result<Option<File>> {
    let path = run.join("lease");
    let file = File::options().read(true).write(true).open(&path)?;
    check_regular_identity(&path, &file, true)?;
    match sys::try_lock(&file, kind) {
        Ok(()) => {},
        Err(fs::TryLockError::WouldBlock) => return Ok(None),
        Err(fs::TryLockError::Error(error)) => return Err(error),
    }
    check_regular_identity(&path, &file, true)?;
    Ok(Some(file))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryIdentity { path: PathBuf, dev: u64, ino: u64 }
impl DirectoryIdentity {
    fn capture(path: PathBuf) -> io::Result<Self> {
        sys::verify_private_dir(&path).map_err(io::Error::other)?;
        let meta = sys::lstat(&path)?;
        Ok(Self { path, dev: meta.dev(), ino: meta.ino() })
    }

    fn matches(&self) -> bool {
        sys::lstat(&self.path).is_ok_and(|m| m.is_dir() && m.dev() == self.dev && m.ino() == self.ino)
    }

    fn remove(&self, names: &[&str]) -> io::Result<u64> {
        if !self.matches() {
            return match sys::lstat(&self.path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
                _ => Err(invalid("broker directory identity changed")),
            };
        }
        let bytes = fsx::allocated_size(&self.path);
        for name in names {
            match fs::remove_file(self.path.join(name)) {
                Ok(()) => {},
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => return Err(error),
            }
        }
        fs::remove_dir(&self.path)?;
        Ok(bytes)
    }
}

struct RunDirectory {
    identity: DirectoryIdentity,
    endpoint: Option<DirectoryIdentity>,
    lease: Option<File>,
    active: bool,
}
impl RunDirectory {
    fn disarm(&mut self) { self.active = false; }

    fn new(path: PathBuf) -> io::Result<Self> {
        Ok(Self { identity: DirectoryIdentity::capture(path)?, endpoint: None, lease: None, active: true })
    }
}
impl Drop for RunDirectory {
    fn drop(&mut self) {
        if self.active {
            // Retain the discovery record whenever the external endpoint could not be removed.
            if self.endpoint.as_ref().is_none_or(|endpoint| endpoint.remove(&["s"]).is_ok()) {
                let _ = self.identity.remove(&["capability.json", "capability.tmp", "lease"]);
            }
        }
        self.lease.take();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StaleRunProof {
    run: DirectoryIdentity,
    endpoint: Option<DirectoryIdentity>,
    metadata: Option<(String, u64, u64, [u8; 32])>,
    lease: Option<(u64, u64)>,
    pub(crate) bytes: u64,
}

fn known_children(path: &Path, allowed: &[&str]) -> io::Result<Option<Vec<String>>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { return Ok(None) };
        if !allowed.contains(&name.as_str()) || names.len() >= allowed.len() { return Ok(None) }
        names.push(name);
    }
    names.sort();
    Ok(Some(names))
}

fn old_run(run: &Path) -> io::Result<bool> {
    Ok(SystemTime::now().duration_since(sys::lstat(run)?.modified()?).is_ok_and(|age| age > RUN_MAX_AGE))
}

fn stale_run(dirs: &Dirs, run: &Path) -> io::Result<Option<(StaleRunProof, Option<File>)>> {
    let run = env::current_dir()?.join(run);
    if run.parent() != Some(absolute_home(dirs)?.join("tmp").as_path()) { return Ok(None) }
    let identity = DirectoryIdentity::capture(run.clone())?;
    if !old_run(&run)? { return Ok(None) }
    let Some(children) = known_children(&run, &["capability.json", "capability.tmp", "lease"])? else { return Ok(None) };
    let lease = if children.iter().any(|name| name == "lease") {
        let Some(lease) = open_run_lease(&run, LockKind::Exclusive)? else { return Ok(None) };
        Some(lease)
    } else if children.is_empty() { None } else { return Ok(None) };
    if !identity.matches() || !old_run(&run)? || known_children(&run, &["capability.json", "capability.tmp", "lease"])? != Some(children.clone()) {
        return Ok(None);
    }
    let mut metadata = None;
    let mut endpoint = None;
    if children.iter().any(|name| name == "capability.json") {
        if children != ["capability.json", "lease"] { return Ok(None) }
        let (bytes, file) = read_regular(&run.join("capability.json"), MAX_REQUEST, true)?;
        let cap: Capability = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        cap.verify_shape(dirs)?;
        if cap.run != run { return Ok(None) }
        endpoint = match sys::lstat(&cap.endpoint()) {
            Ok(_) => {
                verify_endpoint(&cap)?;
                if known_children(&cap.endpoint(), &["s"])?.is_none() { return Ok(None) }
                Some(DirectoryIdentity::capture(cap.endpoint())?)
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        metadata = Some(("capability.json".to_owned(), file.dev(), file.ino(), *blake3::hash(&bytes).as_bytes()));
    } else if children.iter().any(|name| name == "capability.tmp") {
        // An unpublished temporary capability cannot have created an external endpoint.
        let (bytes, file) = read_regular(&run.join("capability.tmp"), MAX_REQUEST, true)?;
        metadata = Some(("capability.tmp".to_owned(), file.dev(), file.ino(), *blake3::hash(&bytes).as_bytes()));
    }
    let held = lease.as_ref().map(|file| check_regular_identity(&run.join("lease"), file, true)).transpose()?;
    let charged = fsx::allocated_size(&run) + endpoint.as_ref().map_or(0, |e| fsx::allocated_size(&e.path));
    let proof = StaleRunProof {
        run: identity, endpoint, metadata,
        lease: held.map(|m| (m.dev(), m.ino())), bytes: charged,
    };
    Ok(Some((proof, lease)))
}

pub(crate) fn stale_run_proof(dirs: &Dirs, run: &Path) -> Option<StaleRunProof> {
    stale_run(dirs, run).ok().flatten().map(|(proof, _lease)| proof)
}

pub(crate) fn retire_stale_run(dirs: &Dirs, run: &Path, expected: &StaleRunProof, freed: &mut u64) -> io::Result<()> {
    // Reopen and lock at execution time: a plan never authorizes deleting a replacement or live lease.
    let Some((now, _lease)) = stale_run(dirs, run).ok().flatten() else { return Ok(()) };
    if &now != expected { return Ok(()) }
    if let Some(endpoint) = &now.endpoint { *freed += endpoint.remove(&["s"])?; }
    *freed += now.run.remove(&["capability.json", "capability.tmp", "lease"])?;
    Ok(())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!("frust-broker-test-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn dirs(&self) -> Dirs { Dirs::at(&self.0) }
    }
    impl Drop for Scratch {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn job_file(dirs: &Dirs) -> PathBuf {
        fs::create_dir_all(dirs.deferred()).unwrap();
        let path = dirs.deferred().join("job-42-123.json");
        fs::write(&path, b"{\"immutable\":true}").unwrap();
        path
    }

    fn prepared(scratch: &Scratch) -> (StoreBrokerLease, Capability) {
        let mut command = Command::new("unused-cargo");
        let mut launch = BrokerLaunch::prepare(&scratch.dirs(), &mut command).unwrap();
        let encoded = command.get_envs().find(|(k, _)| *k == CAPABILITY_ENV).unwrap().1.unwrap();
        let cap = serde_json::from_str(encoded.to_str().unwrap()).unwrap();
        launch.activate();
        (launch.lease.take().unwrap(), cap)
    }

    #[test]
    fn pending_launch_has_no_files_and_early_admission_falls_back() {
        let scratch = Scratch::new();
        let dirs = Dirs::at(scratch.0.join("absent-home"));
        let mut command = Command::new("unused-cargo");
        let launch = BrokerLaunch::prepare(&dirs, &mut command).unwrap();
        let cap = launch.pending.as_ref().unwrap().clone();
        assert!(!dirs.root().exists() && !cap.endpoint().exists());
        assert!(AdmissionPermit::reserve_cap(&dirs, cap.clone()).unwrap().is_none());
        launch.finish();
        assert!(!dirs.root().exists() && !cap.endpoint().exists());
    }

    #[test]
    fn launch_materializes_once_and_finish_closes_its_bootstrap() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let mut launch = BrokerLaunch::prepare(&dirs, &mut Command::new("unused-cargo")).unwrap();
        let cap = launch.pending.as_ref().unwrap().clone();
        launch.activate();
        assert!(launch.pending.is_none() && launch.lease.is_some());
        cap.verify(&dirs).unwrap();
        let identity = DirectoryIdentity::capture(cap.run.clone()).unwrap();
        launch.activate();
        assert!(identity.matches());
        launch.finish();
        assert!(!cap.run.exists() && !cap.endpoint().exists());
    }

    #[test]
    fn second_compile_attempt_materializes_once_without_counting_distinct_keys() {
        let scratch = Scratch::new();
        let dirs = Dirs::at(scratch.0.join("absent-home"));
        let mut launch = BrokerLaunch::prepare(&dirs, &mut Command::new("unused-cargo")).unwrap();
        let cap = launch.pending.as_ref().unwrap().clone();
        launch.note_store_demands(0);
        launch.note_store_demands(1);
        assert!(!dirs.root().exists() && !cap.endpoint().exists());
        assert!(launch.pending.is_some() && launch.lease.is_none());
        launch.note_store_demands(1);
        cap.verify(&dirs).unwrap();
        let identity = DirectoryIdentity::capture(cap.run.clone()).unwrap();
        launch.note_store_demands(u8::MAX);
        assert!(identity.matches());
        launch.finish();
        assert!(!cap.run.exists() && !cap.endpoint().exists());
    }

    #[test]
    fn failed_activation_is_not_retried_after_obstruction_is_repaired() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        fs::write(dirs.tmp(), b"obstruction").unwrap();
        let mut launch = BrokerLaunch::prepare(&dirs, &mut Command::new("unused-cargo")).unwrap();
        let cap = launch.pending.as_ref().unwrap().clone();
        launch.note_store_demands(2);
        assert!(launch.pending.is_none() && launch.lease.is_none());
        sys::rename(&dirs.tmp(), &scratch.0.join("obstruction")).unwrap();
        launch.note_store_demands(2);
        launch.finish();
        assert!(!dirs.tmp().exists() && !cap.endpoint().exists());
    }

    fn fixture_run(dirs: &Dirs) -> (Capability, RunDirectory, Listener) {
        fs::create_dir_all(dirs.tmp()).unwrap();
        let token = store_transport::random_capability().unwrap();
        let cap = Capability {
            version: VERSION, home: dirs.root().to_owned(),
            run: dirs.tmp().join(format!("store-broker-{}-{}", std::process::id(), &token[..16])), token,
        };
        sys::create_dir_with_mode(&cap.run, 0o700).unwrap();
        let mut cleanup = RunDirectory::new(cap.run.clone()).unwrap();
        let mut options = File::options();
        options.read(true).write(true).create_new(true);
        sys::with_mode(&mut options, 0o600);
        let lease = options.open(cap.run.join("lease")).unwrap();
        sys::try_lock(&lease, LockKind::Shared).unwrap();
        cleanup.lease = Some(lease);
        options.open(cap.run.join("capability.json")).unwrap().write_all(&serde_json::to_vec(&cap).unwrap()).unwrap();
        crate::paths::private_temp_dir(Path::new("/tmp")).unwrap();
        sys::create_dir_with_mode(&cap.endpoint(), 0o700).unwrap();
        cleanup.endpoint = Some(DirectoryIdentity::capture(cap.endpoint()).unwrap());
        let listener = Listener::bind_private(&cap.socket()).unwrap();
        (cap, cleanup, listener)
    }

    fn age_run(cap: &Capability) {
        File::open(&cap.run).unwrap().set_modified(SystemTime::now() - RUN_MAX_AGE * 2).unwrap();
    }

    fn admission_job(dirs: &Dirs) -> PathBuf {
        let deps = dirs.root().join("target/debug/deps");
        fs::create_dir_all(&deps).unwrap();
        fs::create_dir_all(dirs.deferred()).unwrap();
        let dep_text = "k.d:\n";
        let dep = deps.join("k.d");
        fs::write(&dep, dep_text).unwrap();
        let meta = sys::stat(&dep).unwrap();
        let ns = |(seconds, nanos): (i64, i64)| (seconds as i128 * 1_000_000_000 + nanos as i128).to_string();
        let job = serde_json::json!({
            "format": 3,
            "context": {"package_version": "test", "compression_requested": false, "immutable_source_roots": []},
            "home": dirs.root(), "cwd": dirs.root(),
            "args": ["rustc", "--crate-name", "k", "--emit=dep-info,link", "--out-dir", deps],
            "prekey": "ab".repeat(20), "unit": null, "trust": "on", "compile_ms": 0,
            "spawned_ns": "0", "captured": "", "watched": [], "extern_hashes": [],
            "names": ["k.d"], "dep_name": "k.d", "dep_text": dep_text,
            "outputs": [["k.d", {"dev": meta.dev(), "ino": meta.ino(), "size": meta.len(), "mtime_ns": ns(meta.mtime()), "ctime_ns": ns(meta.ctime())}]],
            "inputs": [], "commitments": {}, "slot_lock": null, "key_lock": null,
        });
        let bytes = serde_json::to_vec(&job).unwrap();
        wrapper::BrokerJob::decode(&bytes, dirs, None).unwrap();
        let producer = dirs.deferred().join("job-42-longhome.json");
        fs::write(&producer, bytes).unwrap();
        producer
    }

    #[test]
    fn long_home_uses_short_endpoint_for_real_admission() {
        let scratch = Scratch::new();
        let dirs = Dirs::at(scratch.0.join("long-established-home-component".repeat(6)));
        let (cap, _cleanup, listener) = fixture_run(&dirs);
        assert!(dirs.root().as_os_str().as_encoded_bytes().len() > 104);
        assert!(cap.socket().as_os_str().as_encoded_bytes().len() < 104);
        let permit = AdmissionPermit::reserve_cap(&dirs, cap.clone()).unwrap().unwrap();
        let producer = admission_job(&dirs);
        let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
        let receiver_queue = Arc::clone(&queue);
        let receiver_dirs = dirs.clone();
        let (reader, _writer) = store_transport::control_pair().unwrap();
        let receiver = thread::spawn(move || {
            let connection = listener.accept_until(Instant::now() + CLIENT_WAIT).unwrap().unwrap();
            admit(&receiver_dirs, &cap, &connection, &reader, &receiver_queue).unwrap();
        });
        assert!(permit.submit(&producer, None).unwrap());
        receiver.join().unwrap();
        fs::remove_file(producer).unwrap();
        let state = queue.0.lock().unwrap();
        assert_eq!(state.jobs.len(), 1);
        assert!(state.jobs[0].marker.path.exists());
    }

    #[test]
    fn partial_worker_start_failure_joins_workers_without_ack_and_cleans_run() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let (cap, cleanup, listener) = fixture_run(&dirs);
        let permit = AdmissionPermit::reserve_cap(&dirs, cap.clone()).unwrap().unwrap();
        let producer = admission_job(&dirs);
        let deadline = Instant::now() + CLIENT_WAIT;
        let client = Connection::connect_verified(&cap.socket(), deadline).unwrap();
        let request = Request { version: VERSION, token: cap.token.clone(), home: cap.home.clone(), job: producer.clone() };
        client.send(&serde_json::to_vec(&request).unwrap(), &[&permit.reservation], deadline).unwrap();
        let first = listener.accept_until(deadline).unwrap().unwrap();
        let (reader, _writer) = store_transport::control_pair().unwrap();
        let server_dirs = dirs.clone();
        let server_cap = cap.clone();
        let (finished, received) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut queue_ref = std::sync::Weak::new();
            let mut started = 0;
            let result = serve_with_worker_start(
                &server_dirs, &server_cap, &listener, first, &reader, &Config::from_env(),
                (2, |index, queue| {
                    if index == 0 { return Ok(()) }
                    let (state, wake) = &**queue;
                    let mut state = state.lock().unwrap();
                    let deadline = Instant::now() + CLIENT_WAIT;
                    while state.started_workers == 0 {
                        let pause = deadline.saturating_duration_since(Instant::now());
                        if pause.is_zero() { return Err(io::Error::new(io::ErrorKind::TimedOut, "worker did not start")) }
                        state = wake.wait_timeout(state, pause).unwrap().0;
                    }
                    started = state.started_workers;
                    queue_ref = Arc::downgrade(queue);
                    Err(io::Error::other("injected worker startup failure"))
                }),
            );
            let all_joined = queue_ref.upgrade().is_none();
            drop(cleanup);
            finished.send((result, started, all_joined)).unwrap();
        });
        let (result, started, all_joined) = received.recv_timeout(CLIENT_WAIT * 3).unwrap();
        server.join().unwrap();
        assert_eq!(result.unwrap_err().to_string(), "injected worker startup failure");
        assert_eq!(started, 1);
        assert!(all_joined);
        assert!(client.receive(64, Instant::now() + HANDSHAKE).is_err());
        assert!(producer.exists());
        assert_eq!(fs::read_dir(dirs.deferred()).unwrap().count(), 1);
        fs::remove_file(producer).unwrap();
        drop(permit);
        let reservations: Vec<_> = (0..QUEUE_LIMIT).map(|_| reserve_global(&dirs).unwrap().unwrap()).collect();
        assert_eq!(reservations.len(), QUEUE_LIMIT);
        assert!(!cap.run.exists());
        assert!(!cap.endpoint().exists());
    }

    #[test]
    fn gc_preserves_old_shared_leases_during_planning_and_execution() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let (cap, mut cleanup, _listener) = fixture_run(&dirs);
        age_run(&cap);
        let options = crate::gc::GcOptions::new(u64::MAX);
        let plan = crate::gc::plan(&dirs, &options);
        assert!(!plan.removals.iter().any(|r| r.path == cap.run));
        cleanup.lease.take();
        let planned = crate::gc::plan(&dirs, &options);
        assert!(planned.removals.iter().any(|r| r.path == cap.run));
        cleanup.lease = open_run_lease(&cap.run, LockKind::Shared).unwrap();
        let (freed, failures) = crate::gc::execute(&dirs, &planned);
        assert_eq!(freed, 0);
        assert!(failures.is_empty());
        assert!(cap.run.exists());
        assert!(cap.socket().exists());
    }

    #[test]
    fn gc_reclaims_unlocked_old_run_and_exact_endpoint_and_charges_both() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let (cap, mut cleanup, listener) = fixture_run(&dirs);
        cleanup.lease.take();
        drop(listener);
        age_run(&cap);
        let charged = fsx::allocated_size(&cap.run) + fsx::allocated_size(&cap.endpoint());
        let plan = crate::gc::plan(&dirs, &crate::gc::GcOptions::new(u64::MAX));
        let removals: Vec<_> = plan.removals.iter().filter(|r| r.path == cap.run || r.path == cap.endpoint()).collect();
        assert_eq!(removals.len(), 1);
        assert_eq!(removals[0].bytes, charged);
        let (freed, failures) = crate::gc::execute(&dirs, &plan);
        assert!(failures.is_empty());
        assert_eq!(freed, charged);
        assert!(!cap.endpoint().exists());
        assert!(!cap.run.exists());
    }

    #[test]
    fn gc_preserves_a_run_replaced_after_planning_and_its_endpoint() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let (cap, mut cleanup, listener) = fixture_run(&dirs);
        cleanup.lease.take();
        drop(listener);
        age_run(&cap);
        let plan = crate::gc::plan(&dirs, &crate::gc::GcOptions::new(u64::MAX));
        assert!(plan.removals.iter().any(|r| r.path == cap.run));
        let displaced = cap.run.with_extension("preserved");
        sys::rename(&cap.run, &displaced).unwrap();
        sys::create_dir_with_mode(&cap.run, 0o700).unwrap();
        fs::write(cap.run.join("replacement"), b"preserve").unwrap();
        let (freed, failures) = crate::gc::execute(&dirs, &plan);
        assert_eq!(freed, 0);
        assert!(failures.is_empty());
        assert_eq!(fs::read(cap.run.join("replacement")).unwrap(), b"preserve");
        assert!(displaced.join("capability.json").exists());
        assert!(cap.socket().exists());
    }

    #[test]
    fn gc_reclaims_relative_home_runs_without_changing_plan_keys() {
        let scratch = Scratch::new();
        let cwd = env::current_dir().unwrap();
        let mut relative = PathBuf::new();
        for component in cwd.components() {
            if matches!(component, std::path::Component::Normal(_)) { relative.push(".."); }
        }
        relative.push(scratch.0.strip_prefix("/").unwrap());
        let captured_dirs = Dirs::at(cwd.join(&relative));
        let gc_dirs = Dirs::at(&relative);
        let (cap, mut cleanup, listener) = fixture_run(&captured_dirs);
        cleanup.lease.take();
        drop(listener);
        age_run(&cap);
        let plan = crate::gc::plan(&gc_dirs, &crate::gc::GcOptions::new(u64::MAX));
        assert!(plan.removals.iter().any(|r| !r.path.is_absolute() && cwd.join(&r.path) == cap.run));
        let (_, failures) = crate::gc::execute(&gc_dirs, &plan);
        assert!(failures.is_empty());
        assert!(!cap.run.exists());
        assert!(!cap.endpoint().exists());
    }

    #[test]
    fn failed_endpoint_cleanup_retains_discovery_until_gc_can_retry() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let (cap, cleanup, listener) = fixture_run(&dirs);
        drop(listener);
        let blocker = cap.endpoint().join("unknown-child");
        fs::write(&blocker, b"preserve").unwrap();
        drop(cleanup);
        assert!(cap.run.join("capability.json").exists());
        assert!(cap.run.join("lease").exists());
        assert!(blocker.exists());
        age_run(&cap);
        let options = crate::gc::GcOptions::new(u64::MAX);
        let blocked = crate::gc::plan(&dirs, &options);
        assert!(!blocked.removals.iter().any(|r| r.path == cap.run));
        fs::remove_file(blocker).unwrap();
        let retry = crate::gc::plan(&dirs, &options);
        assert!(retry.removals.iter().any(|r| r.path == cap.run));
        let (_, failures) = crate::gc::execute(&dirs, &retry);
        assert!(failures.is_empty());
        assert!(!cap.endpoint().exists());
        assert!(!cap.run.exists());
    }

    #[test]
    fn gc_reclaims_known_incomplete_initialization_and_final_cleanup_states() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        fs::create_dir_all(dirs.tmp()).unwrap();
        let mut runs = Vec::new();
        let mut held = Vec::new();
        for state in 0..3 {
            let token = store_transport::random_capability().unwrap();
            let run = dirs.tmp().join(format!("store-broker-{}-{}", std::process::id(), &token[..16]));
            sys::create_dir_with_mode(&run, 0o700).unwrap();
            let mut options = File::options();
            options.read(true).write(true).create_new(true);
            sys::with_mode(&mut options, 0o600);
            if state > 0 {
                let lease = options.open(run.join("lease")).unwrap();
                sys::try_lock(&lease, LockKind::Shared).unwrap();
                held.push(lease);
            }
            if state == 2 { options.open(run.join("capability.tmp")).unwrap().write_all(b"{partial").unwrap(); }
            File::open(&run).unwrap().set_modified(SystemTime::now() - RUN_MAX_AGE * 2).unwrap();
            runs.push(run);
        }
        let unknown = dirs.tmp().join("store-broker-unknown");
        sys::create_dir_with_mode(&unknown, 0o700).unwrap();
        fs::write(unknown.join("unrecognized"), b"preserve").unwrap();
        File::open(&unknown).unwrap().set_modified(SystemTime::now() - RUN_MAX_AGE * 2).unwrap();
        let options = crate::gc::GcOptions::new(u64::MAX);
        let busy = crate::gc::plan(&dirs, &options);
        assert!(busy.removals.iter().any(|r| r.path == runs[0]));
        assert!(!busy.removals.iter().any(|r| runs[1..].contains(&r.path)));
        drop(held);
        let stale = crate::gc::plan(&dirs, &options);
        assert_eq!(stale.removals.iter().filter(|r| runs.contains(&r.path)).count(), 3);
        let (_, failures) = crate::gc::execute(&dirs, &stale);
        assert!(failures.is_empty());
        assert!(runs.iter().all(|run| !run.exists()));
        assert!(unknown.join("unrecognized").exists());
    }

    #[test]
    fn eight_global_reservations_exclude_and_are_reused() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let mut held: Vec<File> = (0..QUEUE_LIMIT).map(|_| reserve_global(&dirs).unwrap().unwrap()).collect();
        assert!(reserve_global(&Dirs::at(dirs.root())).unwrap().is_none());
        let released = sys::fstat(held.last().unwrap()).unwrap();
        drop(held.pop());
        let reused = reserve_global(&dirs).unwrap().unwrap();
        assert!(same_inode(&released, &sys::fstat(&reused).unwrap()));
        assert!(reserve_global(&dirs).unwrap().is_none());
        assert!(!dirs.deferred().exists());
    }

    #[test]
    fn transferred_reservation_must_name_a_stable_global_file() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let held = reserve_global(&dirs).unwrap().unwrap();
        assert_eq!(validate_reservation(&dirs, &held.try_clone().unwrap()).unwrap(), 0);
        let other_description = File::open(reservation_path(&dirs, 0)).unwrap();
        assert!(validate_reservation(&dirs, &other_description).is_err());
        let wrong = File::create(scratch.0.join("wrong")).unwrap();
        assert!(validate_reservation(&dirs, &wrong).is_err());
    }

    #[test]
    fn independent_marker_survives_producer_removal() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let producer = job_file(&dirs);
        let (bytes, marker) = own_job_marker(&dirs, &producer).unwrap();
        assert_ne!(marker.path, producer);
        fs::remove_file(&producer).unwrap();
        assert_eq!(fs::read(&marker.path).unwrap(), bytes);
        let owned = marker.path.clone();
        assert!(matches!(wrapper::deferred_retirement(&owned).unwrap(), wrapper::DeferredRetirement::Busy));
        let observer = File::open(&owned).unwrap();
        drop(marker);
        assert!(!owned.exists());
        assert!(sys::try_lock(&observer, LockKind::Exclusive).is_ok());
    }

    #[test]
    fn oversized_linked_and_misnamed_metadata_are_refused() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let producer = job_file(&dirs);
        File::options().write(true).open(&producer).unwrap().set_len(MAX_JOB as u64 + 1).unwrap();
        assert!(own_job_marker(&dirs, &producer).is_err());
        fs::write(&producer, b"{}").unwrap();
        let alias = dirs.deferred().join("job-42-alias.json");
        fs::hard_link(&producer, &alias).unwrap();
        assert!(own_job_marker(&dirs, &producer).is_err());
        assert!(own_job_marker(&dirs, &alias).is_err());
        assert!(!valid_job_name(Path::new("job-.json")));
        assert!(!valid_job_name(Path::new("job-1-.json")));
        assert!(!valid_job_name(Path::new("job-1-2.txt")));
        assert!(valid_job_name(Path::new("job-1-2.json")));
        assert!(valid_job_name(Path::new("job-1-broker-abc123.json")));
        assert_eq!(fs::read_dir(dirs.deferred()).unwrap().count(), 2);
    }

    #[test]
    fn frontend_eof_cleans_lazy_bootstrap_without_a_store_process() {
        let scratch = Scratch::new();
        let (lease, cap) = prepared(&scratch);
        cap.verify(&scratch.dirs()).unwrap();
        assert!(cap.socket().exists());
        assert!(!cap.run.join("capability.tmp").exists());
        let begun = Instant::now();
        lease.finish();
        assert!(begun.elapsed() < CLIENT_WAIT);
        assert!(!cap.run.exists());
        assert!(!cap.endpoint().exists());
    }

    #[test]
    fn invalid_capability_cannot_take_a_reservation() {
        let scratch = Scratch::new();
        let (lease, mut cap) = prepared(&scratch);
        cap.token.replace_range(..1, if cap.token.starts_with('a') { "b" } else { "a" });
        assert!(AdmissionPermit::reserve_cap(&scratch.dirs(), cap).unwrap().is_none());
        assert!(!scratch.dirs().locks().exists());
        lease.finish();
    }

    #[test]
    fn execution_slots_cooperate_with_existing_helpers_and_wait_is_bounded() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let mut held = Vec::new();
        for _ in 0..slot_count() {
            held.push(wait_execution_slot(&dirs, Some(Instant::now() + HANDSHAKE)).unwrap().unwrap());
        }
        assert!(wait_execution_slot(&dirs, Some(Instant::now() + Duration::from_millis(15))).unwrap().is_none());
        drop(held.pop());
        assert!(wait_execution_slot(&dirs, Some(Instant::now() + HANDSHAKE)).unwrap().is_some());
    }

    #[test]
    fn production_wait_retains_owned_job_until_late_execution_slot_release() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let producer = job_file(&dirs);
        let (_, marker) = own_job_marker(&dirs, &producer).unwrap();
        let owned_path = marker.path.clone();
        fs::remove_file(producer).unwrap();
        let reservation = reserve_global(&dirs).unwrap().unwrap();
        let reservation_probe = File::open(reservation_path(&dirs, 0)).unwrap();
        let mut held: Vec<_> = (0..slot_count()).map(|_| {
            wait_execution_slot(&dirs, Some(Instant::now() + HANDSHAKE)).unwrap().unwrap()
        }).collect();
        let waiter_dirs = dirs.clone();
        let (finished, received) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            let slot = wait_execution_slot(&waiter_dirs, None);
            drop(marker);
            drop(reservation);
            finished.send(slot).unwrap();
        });
        assert!(wait_execution_slot(&dirs, Some(Instant::now() + Duration::from_millis(25))).unwrap().is_none());
        assert!(matches!(received.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        assert!(owned_path.exists());
        assert!(!try_exclusive(&reservation_probe).unwrap());
        drop(held.pop());
        let acquired = received.recv_timeout(CLIENT_WAIT).unwrap().unwrap().unwrap();
        waiter.join().unwrap();
        assert!(!owned_path.exists());
        assert!(try_exclusive(&reservation_probe).unwrap());
        drop(acquired);
    }

    #[test]
    fn immutable_read_checks_size_before_allocating_and_caps_growth() {
        let scratch = Scratch::new();
        let path = scratch.0.join("metadata");
        fs::write(&path, b"1234").unwrap();
        assert!(read_regular(&path, 3, true).is_err());
        assert_eq!(read_regular(&path, 4, true).unwrap().0, b"1234");
        File::options().write(true).open(&path).unwrap().set_len(u32::MAX as u64).unwrap();
        assert!(read_regular(&path, MAX_JOB, true).is_err());
    }

    #[test]
    fn unknown_ack_returns_inline_while_broker_marker_retains_ownership() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let producer = job_file(&dirs);
        let (cap, _cleanup, listener) = fixture_run(&dirs);
        let permit = AdmissionPermit { cap, reservation: reserve_global(&dirs).unwrap().unwrap() };
        let receiver_dirs = dirs.clone();
        let receiver_producer = producer.clone();
        let broker = thread::spawn(move || {
            let deadline = Instant::now() + CLIENT_WAIT;
            let connection = listener.accept_until(deadline).unwrap().unwrap();
            let (_, files) = connection.receive(MAX_REQUEST, deadline).unwrap();
            assert_eq!(files.len(), 1);
            validate_reservation(&receiver_dirs, &files[0]).unwrap();
            let (_, marker) = own_job_marker(&receiver_dirs, &receiver_producer).unwrap();
            connection.send(b"frust-store-v1:accepted-extra", &[], deadline).unwrap();
            marker
        });
        assert!(!permit.submit(&producer, None).unwrap());
        let marker = broker.join().unwrap();
        fs::remove_file(&producer).unwrap();
        assert!(marker.path.exists());
        assert!(matches!(wrapper::deferred_retirement(&marker.path).unwrap(), wrapper::DeferredRetirement::Busy));
        drop(marker);
    }

    #[test]
    fn incomplete_connection_has_a_fixed_handshake_deadline() {
        let scratch = Scratch::new();
        let dirs = scratch.dirs();
        let (cap, _cleanup, listener) = fixture_run(&dirs);
        let deadline = Instant::now() + CLIENT_WAIT;
        let _idle_client = Connection::connect_verified(&cap.socket(), deadline).unwrap();
        let server = listener.accept_until(deadline).unwrap().unwrap();
        let (reader, _writer) = store_transport::control_pair().unwrap();
        let queue = Arc::new((Mutex::new(QueueState::default()), Condvar::new()));
        let begun = Instant::now();
        assert!(admit(&dirs, &cap, &server, &reader, &queue).is_err());
        assert!(begun.elapsed() < Duration::from_millis(500));
        assert!(queue.0.lock().unwrap().jobs.is_empty());
    }
}
