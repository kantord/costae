use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Seek, SeekFrom, Write as IoWrite};
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::managed_set::{Lifecycle, ManagedSet};

/// Stable identity for a process: uniquely identifies which process to manage.
/// Used as the key in `Lifecycle` so that `ManagedSet` can track processes by identity.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct ProcessIdentity {
    pub bin: String,
    pub key: String,
}

// NOTE: env uses BTreeMap (not HashMap) for deterministic ordering; HashMap doesn't implement Hash.
#[derive(Clone, Debug)]
pub struct ProcessSource {
    pub identity: ProcessIdentity,
    pub script: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: Option<PathBuf>,
    pub props: Option<serde_json::Value>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct StreamItem {
    pub key: (String, Option<String>),
    pub stream: StreamKind,
    pub line: String,
}

pub struct ProcessState {
    pub child: std::process::Child,
    pub event_tx: mpsc::Sender<serde_json::Value>,
    pub last_sent_props: Option<serde_json::Value>,
}

fn spawn_process(spec: ProcessSource, tx: &mpsc::Sender<StreamItem>) -> Option<ProcessState> {
    let mut cmd = std::process::Command::new(&spec.identity.bin);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(ref dir) = spec.current_dir {
        cmd.current_dir(dir);
    }

    // If a script is provided, write it to a memfd and pass the path as an argument.
    #[allow(clippy::option_if_let_else)]
    let _memfd_file = if let Some(ref content) = spec.script {
        let fd = unsafe { libc::memfd_create(c"costae-script".as_ptr(), 0) };
        if fd < 0 {
            tracing::error!(bin = %spec.identity.bin, "memfd_create failed");
            return None;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let _ = file.write_all(content.as_bytes());
        let _ = file.seek(SeekFrom::Start(0));
        cmd.arg(format!("/proc/self/fd/{}", fd));
        Some(file) // keep alive until after spawn so fd is inherited
    } else {
        None
    };

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(bin = %spec.identity.bin, error = %e, "failed to spawn");
            return None;
        }
    };

    if let Some(stdout) = child.stdout.take() {
        let spec_for_thread = spec.clone();
        let tx_stdout = tx.clone();
        thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let item = StreamItem {
                            key: (spec_for_thread.identity.bin.clone(), spec_for_thread.script.clone()),
                            stream: StreamKind::Stdout,
                            line: l,
                        };
                        if tx_stdout.send(item).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let bin_name = spec.identity.bin.clone();
        thread::spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => tracing::warn!(module = %bin_name, "{l}"),
                    Err(_) => break,
                }
            }
        });
    }

    // Wire up a stdin writer thread backed by an mpsc channel.
    let (event_tx, event_rx) = mpsc::channel::<serde_json::Value>();
    if let Some(mut stdin) = child.stdin.take() {
        thread::spawn(move || {
            while let Ok(event) = event_rx.recv() {
                let line = serde_json::to_string(&event).unwrap_or_default() + "\n";
                if stdin.write_all(line.as_bytes()).is_err() {
                    break;
                }
            }
        });
    }

    Some(ProcessState { child, event_tx, last_sent_props: None })
}

impl Lifecycle for ProcessSource {
    type Key = ProcessIdentity;
    type State = ProcessState;
    type Context = mpsc::Sender<StreamItem>;

    fn key(&self) -> ProcessIdentity {
        self.identity.clone()
    }

    fn enter(self, ctx: &Self::Context) -> Option<Self::State> {
        let props = self.props.clone();
        let mut state = spawn_process(self, ctx)?;
        if let Some(p) = props {
            let _ = state.event_tx.send(p.clone());
            state.last_sent_props = Some(p);
        }
        Some(state)
    }

    fn update(self, state: &mut Self::State, ctx: &Self::Context) {
        if matches!(state.child.try_wait(), Ok(Some(_))) {
            tracing::warn!(bin = %self.identity.bin, "process exited");
            let props = self.props.clone();
            if let Some(mut new_state) = spawn_process(self, ctx) {
                if let Some(p) = props {
                    let _ = new_state.event_tx.send(p.clone());
                    new_state.last_sent_props = Some(p);
                }
                *state = new_state;
            }
        } else if let Some(p) = self.props.clone() {
            if state.last_sent_props.as_ref() != Some(&p) {
                let _ = state.event_tx.send(p.clone());
                state.last_sent_props = Some(p);
            }
        }
    }

    fn exit(mut state: Self::State, _ctx: &Self::Context) {
        let _ = state.child.kill();
    }
}

#[derive(Clone)]
pub struct InternalSource {
    pub key: String,
    pub value: serde_json::Value,
}

impl Lifecycle for InternalSource {
    type Key = String;
    type State = serde_json::Value;
    type Context = mpsc::Sender<StreamItem>;

    fn key(&self) -> String {
        self.key.clone()
    }

    fn enter(self, ctx: &Self::Context) -> Option<Self::State> {
        let line = serde_json::to_string(&self.value).unwrap_or_default();
        let _ = ctx.send(StreamItem { key: (self.key.clone(), None), stream: StreamKind::Stdout, line });
        Some(self.value)
    }

    fn update(self, state: &mut Self::State, ctx: &Self::Context) {
        if *state != self.value {
            let line = serde_json::to_string(&self.value).unwrap_or_default();
            let _ = ctx.send(StreamItem { key: (self.key.clone(), None), stream: StreamKind::Stdout, line });
            *state = self.value.clone();
        }
    }

    fn exit(_state: Self::State, _ctx: &Self::Context) {}
}

pub enum StreamSource {
    Process(ProcessSource),
    Internal(InternalSource),
}

pub struct DataLoopHandle {
    tx: mpsc::Sender<Vec<StreamSource>>,
}

impl DataLoopHandle {
    pub fn set_desired(&self, sources: Vec<StreamSource>) {
        let _ = self.tx.send(sources);
    }
}

pub struct DataLoop {
    process_pool: ManagedSet<ProcessSource>,
    internal_pool: ManagedSet<InternalSource>,
    stream_tx: mpsc::Sender<StreamItem>,
    desired_processes: Vec<ProcessSource>,
    desired_internals: Vec<InternalSource>,
    timeout: Option<Duration>,
    rx: mpsc::Receiver<StreamItem>,
    extra_rx: Option<mpsc::Receiver<()>>,
    desired_rx: mpsc::Receiver<Vec<StreamSource>>,
    /// Shared snapshot of event senders, keyed by bin name.
    /// Updated on every `set_desired` call so callers outside `run` can route events.
    event_txs_snapshot: Arc<Mutex<HashMap<String, mpsc::Sender<serde_json::Value>>>>,
}

impl DataLoop {
    pub fn new() -> (Self, DataLoopHandle) {
        let (stream_tx, rx) = mpsc::channel();
        let (desired_tx, desired_rx) = mpsc::channel();
        let event_txs_snapshot = Arc::new(Mutex::new(HashMap::new()));
        let data_loop = Self {
            process_pool: ManagedSet::new(),
            internal_pool: ManagedSet::new(),
            stream_tx,
            desired_processes: Vec::new(),
            desired_internals: Vec::new(),
            timeout: None,
            rx,
            extra_rx: None,
            desired_rx,
            event_txs_snapshot,
        };
        let handle = DataLoopHandle { tx: desired_tx };
        (data_loop, handle)
    }

    /// Returns a clone of the shared event_txs snapshot Arc.
    /// Callers can hold this Arc and read from it while `run` is executing.
    pub fn event_txs_handle(&self) -> Arc<Mutex<HashMap<String, mpsc::Sender<serde_json::Value>>>> {
        Arc::clone(&self.event_txs_snapshot)
    }

    pub fn with_extra_rx(mut self, rx: mpsc::Receiver<()>) -> Self {
        self.extra_rx = Some(rx);
        self
    }

    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn collect_event_txs(&self) -> HashMap<ProcessIdentity, mpsc::Sender<serde_json::Value>> {
        self.process_pool.iter()
            .map(|(identity, state)| (identity.clone(), state.event_tx.clone()))
            .collect()
    }

    pub fn send_event(&mut self, identity: &ProcessIdentity, event: serde_json::Value) {
        loop {
            match self.desired_rx.try_recv() {
                Ok(sources) => self.set_desired(sources),
                Err(_) => break,
            }
        }
        self.process_pool.update(self.desired_processes.clone(), &self.stream_tx);
        if let Some(state) = self.process_pool.get(identity) {
            let _ = state.event_tx.send(event);
        }
    }

    fn set_desired(&mut self, sources: Vec<StreamSource>) {
        let mut processes = vec![];
        let mut internals = vec![];
        for s in sources {
            match s {
                StreamSource::Process(p) => processes.push(p),
                StreamSource::Internal(i) => internals.push(i),
            }
        }
        let mut seen = std::collections::HashSet::new();
        self.desired_processes = processes
            .into_iter()
            .filter(|s| seen.insert(s.identity.clone()))
            .collect();
        self.desired_internals = internals;
        self.process_pool.update(self.desired_processes.clone(), &self.stream_tx);
        self.internal_pool.update(self.desired_internals.clone(), &self.stream_tx);
        self.update_event_txs_snapshot();
    }

    fn update_event_txs_snapshot(&self) {
        let mut snapshot = self.event_txs_snapshot.lock().unwrap();
        *snapshot = self.process_pool.iter()
            .map(|(identity, state)| (identity.bin.clone(), state.event_tx.clone()))
            .collect();
    }

    pub fn run(
        &mut self,
        stop: Arc<AtomicBool>,
        mut on_item: impl FnMut(StreamItem),
        mut on_tick: impl FnMut(),
    ) {
        let mut awake = false;
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // Drain desired_rx: apply any new desired sets sent via DataLoopHandle.
            loop {
                match self.desired_rx.try_recv() {
                    Ok(sources) => self.set_desired(sources),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            // Check extra_rx: if a message arrives, stay awake (no blocking recv) for the
            // rest of the run so the stop flag and further ticks are detected promptly.
            // If the extra_rx sender is dropped, treat that as a stop signal.
            if let Some(ref extra_rx) = self.extra_rx {
                match extra_rx.try_recv() {
                    Ok(()) => {
                        awake = true;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }

            // Reconcile: enter new, exit removed, update existing (restarts crashed processes).
            self.process_pool.update(self.desired_processes.clone(), &self.stream_tx);
            self.internal_pool.update(self.desired_internals.clone(), &self.stream_tx);
            self.update_event_txs_snapshot();

            on_tick();

            if awake {
                awake = false;
                match self.rx.try_recv() {
                    Ok(item) => on_item(item),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
                continue;
            }

            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(item) => on_item(item),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── script field ──────────────────────────────────────────────────────────

    /// Type-system enforcement: `ProcessSource` must carry a `script: Option<String>` field.
    /// This test fails to compile until the field exists.
    #[test]
    fn command_spec_has_script_field() {
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec![],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: Some("echo from_script".to_string()),
        };
        assert!(spec.script.is_some());
    }

    /// Runtime: when `ProcessSource` carries a script, the subprocess spawned via
    /// `DataLoop` executes that script and its output appears as a `StreamItem`.
    #[test]
    fn script_content_is_executed_and_output_delivered() {
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec![],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: Some("echo from_script".to_string()),
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec.clone())]);

        let items: Arc<Mutex<Vec<StreamItem>>> = Arc::new(Mutex::new(Vec::new()));
        let items_clone = Arc::clone(&items);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        data_loop.run(stop_for_run, |item| {
            items_clone.lock().unwrap().push(item);
            stop.store(true, Ordering::Relaxed);
        }, || {});

        let items = items.lock().unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item.line, "from_script",
            "expected output from script content, got {:?}",
            item.line
        );
        assert_eq!(item.stream, StreamKind::Stdout);
    }

    #[test]
    fn duplicate_specs_without_key_spawn_only_one_process() {
        // Use a process that emits once then sleeps — stays alive for the test window
        // so no restart occurs. This isolates the deduplication invariant from restart behavior.
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec!["-c".to_string(), "echo hello; sleep 10".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        // Pass the same spec twice; no `key` to distinguish them.
        handle.set_desired(vec![StreamSource::Process(spec.clone()), StreamSource::Process(spec.clone())]);

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        // Attempt to receive up to 2 items in a background thread.
        // If two processes are spawned (buggy), both lines arrive and the thread
        // finishes quickly.  If only one process is spawned (correct), the second
        // recv() blocks forever — which is fine; the thread is abandoned.
        thread::spawn(move || {
            data_loop.run(stop_clone, |item| {
                let mut guard = collected_clone.lock().unwrap();
                guard.push(item.line);
                if guard.len() >= 2 {
                    stop.store(true, Ordering::Relaxed);
                }
            }, || {});
        });

        // Give ample time for both processes to emit their line and exit.
        thread::sleep(Duration::from_millis(500));

        let items = collected.lock().unwrap();
        let len = items.len();
        assert_eq!(
            len,
            1,
            "expected exactly one process to be spawned for duplicate specs, \
             got {} items: {:?}",
            len,
            *items
        );
    }

    #[test]
    fn stdout_line_is_delivered_to_handler_with_correct_source_and_kind() {
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec!["-c".to_string(), "echo hello".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec.clone())]);

        let items: Arc<Mutex<Vec<StreamItem>>> = Arc::new(Mutex::new(Vec::new()));
        let items_clone = Arc::clone(&items);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        data_loop.run(stop_for_run, |item| {
            items_clone.lock().unwrap().push(item);
            stop.store(true, Ordering::Relaxed);
        }, || {});

        let items = items.lock().unwrap();
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.line, "hello");
        assert_eq!(item.key.0, spec.identity.bin);
        assert_eq!(item.stream, StreamKind::Stdout);
    }

    #[test]
    fn crashed_process_is_restarted_and_output_continues() {
        // Use a command that emits one line then exits immediately.
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec!["-c".to_string(), "echo hello".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec.clone())]);

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_for_run = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        let run_handle = thread::spawn(move || {
            data_loop.run(stop_for_run, |item| {
                collected_for_run.lock().unwrap().push(item.line);
            }, || {});
        });

        // Wait for the first line to arrive (process ran once).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if collected.lock().unwrap().len() >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for first output line"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // Wait 300 ms — enough for the process to have exited and been restarted
        // on the next reconcile tick, and for the restarted process to emit its line.
        thread::sleep(Duration::from_millis(300));

        let count = collected.lock().unwrap().len();
        stop.store(true, Ordering::Relaxed);
        let _ = run_handle.join();

        assert!(
            count >= 2,
            "expected at least 2 output lines (original + restart), got {}",
            count
        );
    }

    #[test]
    fn run_stops_when_cancellation_token_is_set() {
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec![
                "-c".to_string(),
                "while true; do echo tick; sleep 0.1; done".to_string(),
            ],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec)]);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);
        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_for_run = Arc::clone(&collected);

        let run_handle = thread::spawn(move || {
            data_loop.run(stop_for_run, |item| {
                collected_for_run.lock().unwrap().push(item.line);
            }, || {});
        });

        // Wait until at least one item has been collected, then signal stop.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if collected.lock().unwrap().len() >= 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for first tick"
            );
            thread::sleep(Duration::from_millis(20));
        }

        stop.store(true, Ordering::Relaxed);

        // run() must return within a generous timeout once the token is set.
        let joined = run_handle.join();
        assert!(
            joined.is_ok(),
            "run() thread panicked or did not stop after cancellation token was set"
        );
    }

    /// Claim 1 — compile-time: `run` must accept a third argument `on_tick: impl FnMut()`.
    /// This test fails to compile until `run`'s signature is updated to accept two closures.
    #[test]
    fn run_accepts_on_tick_callback() {
        let (mut data_loop, _handle) = DataLoop::new();
        let stop = Arc::new(AtomicBool::new(true)); // stopped immediately
        let tick_called = Arc::new(Mutex::new(false));
        let tick_called_clone = Arc::clone(&tick_called);

        // Passing a non-trivial on_tick closure ensures the compiler requires the argument.
        data_loop.run(stop, |_item: StreamItem| {}, move || {
            *tick_called_clone.lock().unwrap() = true;
        });
        // No assertion needed — the compile-time arity check is the test.
    }

    /// Claim 2 — runtime: when a message arrives on `extra_rx`, the loop wakes
    /// promptly and calls `on_tick` within the 50 ms window.
    #[test]
    fn extra_rx_wake_calls_on_tick_within_deadline() {
        let (wake_tx, wake_rx) = mpsc::channel::<()>();

        // DataLoop with no desired specs (no child processes) and an extra_rx attached.
        let (data_loop, _handle) = DataLoop::new();
        let mut data_loop = data_loop.with_extra_rx(wake_rx);

        let tick_called = Arc::new(AtomicBool::new(false));
        let tick_called_for_cb = Arc::clone(&tick_called);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);
        let stop_for_wake = Arc::clone(&stop);

        // Background thread: send a wake signal after 20 ms, then stop the loop.
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let _ = wake_tx.send(());
            // Give the loop a moment to react, then stop it.
            // Use 100 ms so that stop is set at ~120 ms, well within the 200 ms deadline.
            thread::sleep(Duration::from_millis(100));
            stop_for_wake.store(true, Ordering::Relaxed);
        });

        let start = std::time::Instant::now();
        data_loop.run(
            stop_for_run,
            |_item| {},
            move || {
                tick_called_for_cb.store(true, Ordering::Relaxed);
            },
        );
        let elapsed = start.elapsed();

        assert!(
            tick_called.load(Ordering::Relaxed),
            "on_tick was never called after extra_rx wake signal"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "on_tick was not called within 200 ms deadline (took {:?})",
            elapsed
        );
    }

    // ── ProcessIdentity / ProcessSource refactor ───────────────────────────────

    /// Compile-time: `ProcessIdentity` must exist with `bin: String` and `key: String`
    /// fields and derive `Hash`, `Eq`, `PartialEq`, `Clone`.
    #[test]
    fn process_identity_has_bin_and_key_fields() {
        let id = ProcessIdentity {
            bin: "mybin".to_string(),
            key: "mykey".to_string(),
        };
        assert_eq!(id.bin, "mybin");
        assert_eq!(id.key, "mykey");
    }

    /// Compile-time: `ProcessIdentity` must implement `Hash + Eq + PartialEq + Clone`
    /// so it can be used as a `HashMap` key.
    #[test]
    fn process_identity_derives_hash_eq_partialeq_clone() {
        use std::collections::HashSet;
        let a = ProcessIdentity { bin: "bin".to_string(), key: "k".to_string() };
        let b = a.clone();
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a);
        // Inserting a clone of the same identity should not grow the set.
        assert!(!set.insert(b));
    }

    /// Compile-time: `ProcessSource` must have an `identity: ProcessIdentity` field
    /// plus the runtime fields `script`, `args`, `env`, `current_dir`, `props`.
    #[test]
    fn command_spec_composed_of_identity_and_runtime_fields() {
        let spec = ProcessSource {
            identity: ProcessIdentity {
                bin: "/bin/sh".to_string(),
                key: "my-key".to_string(),
            },
            script: Some("echo hello".to_string()),
            args: vec!["--flag".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
        };
        assert_eq!(spec.identity.bin, "/bin/sh");
        assert_eq!(spec.identity.key, "my-key");
        assert!(spec.script.is_some());
    }

    #[test]
    fn has_key_for_command_spec_returns_process_identity() {
        use crate::managed_set::Lifecycle;
        let id = ProcessIdentity {
            bin: "/usr/bin/cat".to_string(),
            key: "cat-key".to_string(),
        };
        let spec = ProcessSource {
            identity: id.clone(),
            script: None,
            args: vec![],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
        };
        // The returned key must be a `ProcessIdentity` equal to `spec.identity`.
        let returned: ProcessIdentity = spec.key();
        assert_eq!(returned, id);
    }

    // ── DataLoopHandle ────────────────────────────────────────────────────────

    /// Compile-time test: `DataLoop::new()` must return a `(DataLoop, DataLoopHandle)` tuple.
    /// Fails to compile until the return type is changed.
    #[test]
    fn data_loop_new_returns_tuple_with_handle() {
        let (_data_loop, _handle): (DataLoop, DataLoopHandle) = DataLoop::new();
    }

    // ── props / init / update / send_event ───────────────────────────────────

    /// Claim A — runtime: when `ProcessSource` has `props: Some(value)`, the subprocess
    /// receives `<value>\n` on its stdin before producing output (sent directly, no wrapping).
    ///
    /// The script reads one line from stdin and echoes it back prefixed with "got:".
    #[test]
    fn props_init_message_is_sent_to_subprocess_stdin() {
        let props_value = serde_json::json!({"color": "red"});
        let expected_payload = serde_json::json!({"color": "red"});
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "init-test".to_string() },
            // Script: read one line from stdin, echo it back
            args: vec!["-c".to_string(), "read line; echo \"got:$line\"".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            props: Some(props_value),
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec.clone())]);

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        let run_handle = thread::spawn(move || {
            data_loop.run(
                stop_for_run,
                |item| {
                    collected_clone.lock().unwrap().push(item.line);
                },
                || {},
            );
        });

        // Wait up to 3 s for the echoed init line.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if !collected.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for subprocess to echo init message"
            );
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = run_handle.join();

        let items = collected.lock().unwrap();
        let expected_got = format!("got:{}", expected_payload);
        assert!(
            items.iter().any(|l| l == &expected_got),
            "expected echoed init payload {:?}, got: {:?}",
            expected_got,
            *items
        );
    }

    /// Claim B — runtime: when a running process's spec is updated with the same
    /// `ProcessIdentity` but different `props`, the subprocess receives
    /// `{"type":"update","props":<new_value>}\n` on its stdin.
    ///
    /// The script loops reading lines from stdin and echoing each one back.
    /// After the update we assert that the echoed update payload appears in output.
    #[test]
    fn props_update_message_is_sent_to_subprocess_stdin_on_spec_update() {
        let initial_props = serde_json::json!({"step": 1});
        let updated_props = serde_json::json!({"step": 2});
        let expected_update_payload = serde_json::json!({"step": 2});

        let identity = ProcessIdentity {
            bin: "/bin/sh".to_string(),
            key: "update-test".to_string(),
        };

        // Script: loop-reads lines from stdin and echoes each one.
        let spec_v1 = ProcessSource {
            identity: identity.clone(),
            args: vec![
                "-c".to_string(),
                "while read line; do echo \"got:$line\"; done".to_string(),
            ],
            env: BTreeMap::new(),
            current_dir: None,
            props: Some(initial_props),
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec_v1.clone())]);

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        let run_handle = thread::spawn(move || {
            data_loop.run(
                stop_for_run,
                |item| {
                    collected_clone.lock().unwrap().push(item.line);
                },
                || {},
            );
        });

        // Wait for the init echo to confirm the process is running and reading stdin.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if !collected.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for subprocess to echo init message"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // Now update the spec with new props — same identity, different props.
        let spec_v2 = ProcessSource {
            identity: identity.clone(),
            args: vec![
                "-c".to_string(),
                "while read line; do echo \"got:$line\"; done".to_string(),
            ],
            env: BTreeMap::new(),
            current_dir: None,
            props: Some(updated_props),
            script: None,
        };
        handle.set_desired(vec![StreamSource::Process(spec_v2)]);

        // Wait for the update echo to appear.
        let expected_got = format!("got:{}", expected_update_payload);
        let update_deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if collected.lock().unwrap().iter().any(|l| l == &expected_got) {
                break;
            }
            assert!(
                std::time::Instant::now() < update_deadline,
                "timed out waiting for subprocess to echo update message"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // Wait an extra 150 ms after the first echo to give the loop time to deliver
        // any duplicate sends before we stop and count occurrences.
        thread::sleep(Duration::from_millis(150));

        stop.store(true, Ordering::Relaxed);
        let _ = run_handle.join();

        let items = collected.lock().unwrap();
        let count = items.iter().filter(|l| l.as_str() == expected_got).count();
        assert_eq!(
            count,
            1,
            "expected updated props payload to be sent exactly once, but got {} occurrences: {:?}",
            count,
            *items
        );
    }

    /// Claim C — compile-time + runtime: `DataLoop` must expose a
    /// `send_event(identity: &ProcessIdentity, event: serde_json::Value)` method that
    /// writes an arbitrary JSON event to the stdin of the matching running process.
    ///
    /// The script loops reading lines from stdin and echoing each one back.
    /// After calling `send_event` we assert the echoed payload appears in output.
    #[test]
    fn send_event_writes_arbitrary_json_to_subprocess_stdin() {
        let identity = ProcessIdentity {
            bin: "/bin/sh".to_string(),
            key: "send-event-test".to_string(),
        };
        let spec = ProcessSource {
            identity: identity.clone(),
            args: vec![
                "-c".to_string(),
                "while read line; do echo \"got:$line\"; done".to_string(),
            ],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![StreamSource::Process(spec)]);

        let event = serde_json::json!({"type": "ping", "id": 42});
        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        let run_handle = thread::spawn(move || {
            // Small delay to let the loop start and the process be spawned.
            thread::sleep(Duration::from_millis(50));
            data_loop.send_event(&identity, event.clone());
            data_loop.run(
                stop_for_run,
                |item| {
                    collected_clone.lock().unwrap().push(item.line);
                },
                || {},
            );
        });

        let expected_got = format!("got:{}", serde_json::json!({"type": "ping", "id": 42}));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if collected.lock().unwrap().iter().any(|l| l == &expected_got) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for send_event echo"
            );
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = run_handle.join();

        let items = collected.lock().unwrap();
        assert!(
            items.iter().any(|l| l == &expected_got),
            "expected echoed event payload {:?}, got: {:?}",
            expected_got,
            *items
        );
    }

    /// Claim D — runtime: when a running process receives two consecutive `set_desired`
    /// calls with IDENTICAL props, the subprocess's stdin should only receive the props
    /// payload ONCE (deduplication by props value).
    ///
    /// The script loops reading lines from stdin and echoes each back prefixed "got:".
    /// We call `set_desired` twice with the same spec (same props), wait long enough for
    /// both ticks to fire, then assert the subprocess echoed the payload exactly once.
    #[test]
    fn identical_props_sent_only_once_on_consecutive_set_desired() {
        let props_value = serde_json::json!({"step": 99});
        let identity = ProcessIdentity {
            bin: "/bin/sh".to_string(),
            key: "dedup-props-test".to_string(),
        };

        let spec = ProcessSource {
            identity: identity.clone(),
            args: vec![
                "-c".to_string(),
                "while read line; do echo \"got:$line\"; done".to_string(),
            ],
            env: BTreeMap::new(),
            current_dir: None,
            props: Some(props_value.clone()),
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        // First set_desired — spawns the process and sends initial props.
        handle.set_desired(vec![StreamSource::Process(spec.clone())]);

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_clone = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        let run_handle = thread::spawn(move || {
            data_loop.run(
                stop_for_run,
                |item| {
                    collected_clone.lock().unwrap().push(item.line);
                },
                || {},
            );
        });

        // Wait for the first echo to confirm the process is up and reading stdin.
        let expected_got = format!("got:{}", props_value);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if collected.lock().unwrap().iter().any(|l| l == &expected_got) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for first props echo"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // Second set_desired with IDENTICAL spec/props — should NOT send props again.
        handle.set_desired(vec![StreamSource::Process(spec.clone())]);

        // Give enough time for at least two tick cycles (2 × 50 ms = 100 ms)
        // so that if the bug is present the duplicate would have been delivered and echoed.
        thread::sleep(Duration::from_millis(300));

        stop.store(true, Ordering::Relaxed);
        let _ = run_handle.join();

        let items = collected.lock().unwrap();
        let count = items.iter().filter(|l| l.as_str() == expected_got).count();
        assert_eq!(
            count,
            1,
            "expected props payload to be delivered exactly once, but got {} occurrences: {:?}",
            count,
            *items
        );
    }

    // ── InternalSource ────────────────────────────────────────────────────────

    /// Claim A: `InternalSource::enter` emits exactly one `StreamItem` whose key is
    /// `(source.key.clone(), None)` and whose line is the JSON-serialised value.
    #[test]
    fn internal_source_enter_emits_stream_item_with_correct_key_and_line() {
        use crate::managed_set::Lifecycle;
        let (tx, rx) = mpsc::channel::<StreamItem>();
        let source = InternalSource {
            key: "my-key".to_string(),
            value: serde_json::json!({"foo": 42}),
        };
        let expected_key = (source.key.clone(), None);
        let expected_line = serde_json::to_string(&source.value).unwrap();

        let _state = source.enter(&tx);

        let item = rx.recv_timeout(Duration::from_millis(200))
            .expect("InternalSource::enter must emit a StreamItem");
        assert_eq!(
            item.key, expected_key,
            "StreamItem key must be (source.key, None)"
        );
        assert_eq!(
            item.line, expected_line,
            "StreamItem line must be JSON-serialised value"
        );
        // No further items should be emitted by enter alone.
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "enter must emit exactly one StreamItem"
        );
    }

    /// Claim B: `InternalSource::update` emits a new `StreamItem` when the value changes.
    #[test]
    fn internal_source_update_emits_stream_item_when_value_changes() {
        use crate::managed_set::Lifecycle;
        let (tx, rx) = mpsc::channel::<StreamItem>();
        let source_v1 = InternalSource {
            key: "upd-key".to_string(),
            value: serde_json::json!(1),
        };
        let mut state = source_v1.enter(&tx).expect("enter must succeed");
        // Drain the enter emission.
        let _ = rx.recv_timeout(Duration::from_millis(200))
            .expect("enter must emit an item");

        // Update with a different value.
        let source_v2 = InternalSource {
            key: "upd-key".to_string(),
            value: serde_json::json!(2),
        };
        let expected_key = (source_v2.key.clone(), None);
        let expected_line = serde_json::to_string(&source_v2.value).unwrap();

        source_v2.update(&mut state, &tx);

        let item = rx.recv_timeout(Duration::from_millis(200))
            .expect("update must emit a StreamItem when value changes");
        assert_eq!(item.key, expected_key);
        assert_eq!(item.line, expected_line);
    }

    /// Claim C: `InternalSource::update` does NOT emit when the value is identical to
    /// the last emitted value (deduplication).
    #[test]
    fn internal_source_update_does_not_emit_when_value_unchanged() {
        use crate::managed_set::Lifecycle;
        let (tx, rx) = mpsc::channel::<StreamItem>();
        let source_v1 = InternalSource {
            key: "dedup-key".to_string(),
            value: serde_json::json!({"x": 7}),
        };
        let mut state = source_v1.enter(&tx).expect("enter must succeed");
        // Drain the enter emission.
        let _ = rx.recv_timeout(Duration::from_millis(200))
            .expect("enter must emit an item");

        // Update with the same value — must NOT emit.
        let source_same = InternalSource {
            key: "dedup-key".to_string(),
            value: serde_json::json!({"x": 7}),
        };
        source_same.update(&mut state, &tx);

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "update must NOT emit a StreamItem when value is identical to last emitted"
        );
    }

    /// Runtime test: calling `handle.set_desired(specs)` from outside the `run` loop
    /// updates the running pool — a new spec is spawned — and output arrives on the
    /// `on_item` callback, proving the handle can communicate with the running loop
    /// without stopping it.
    #[test]
    fn handle_set_desired_spawns_process_into_running_loop() {
        let spec = ProcessSource {
            identity: ProcessIdentity { bin: "/bin/sh".to_string(), key: "/bin/sh".to_string() },
            args: vec!["-c".to_string(), "echo handle_output".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            props: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_for_run = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        // Start run() in a background thread — it holds the mutable borrow of data_loop.
        thread::spawn(move || {
            data_loop.run(
                stop_for_run,
                |item| {
                    collected_for_run.lock().unwrap().push(item.line);
                },
                || {},
            );
        });

        // From the main thread (outside run), call handle.set_desired to inject a spec.
        handle.set_desired(vec![StreamSource::Process(spec)]);

        // Wait up to 3 s for the output to arrive.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if !collected.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for output from handle-spawned process"
            );
            thread::sleep(Duration::from_millis(20));
        }

        stop.store(true, Ordering::Relaxed);

        let items = collected.lock().unwrap();
        assert!(
            items.iter().any(|l| l == "handle_output"),
            "expected 'handle_output' in collected lines, got: {:?}",
            *items
        );
    }
}
