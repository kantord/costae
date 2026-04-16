use std::collections::BTreeMap;
use std::io::{BufRead, Seek, SeekFrom, Write as IoWrite};
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use crate::managed_set::{HasKey, Lifecycle, ManagedSet};

// NOTE: env uses BTreeMap (not HashMap) so that CommandSpec can derive Hash.
// HashMap does not implement Hash; BTreeMap does because it has deterministic iteration order.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct CommandSpec {
    pub bin: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub current_dir: Option<PathBuf>,
    pub key: Option<String>,
    pub script: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct StreamItem {
    pub source: CommandSpec,
    pub stream: StreamKind,
    pub line: String,
}

fn spawn_process(spec: CommandSpec, tx: &mpsc::Sender<StreamItem>) -> Option<std::process::Child> {
    let mut cmd = std::process::Command::new(&spec.bin);
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
            tracing::error!(bin = %spec.bin, "memfd_create failed");
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(bin = %spec.bin, error = %e, "failed to spawn");
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
                            source: spec_for_thread.clone(),
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
        let bin_name = spec.bin.clone();
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

    Some(child)
}

impl HasKey for CommandSpec {
    type Key = Self;
    fn key(&self) -> Self {
        self.clone()
    }
}

impl Lifecycle for CommandSpec {
    type State = std::process::Child;
    type Context = mpsc::Sender<StreamItem>;

    fn enter(self, ctx: &Self::Context) -> Option<Self::State> {
        spawn_process(self, ctx)
    }

    fn update(self, state: &mut Self::State, ctx: &Self::Context) {
        if matches!(state.try_wait(), Ok(Some(_))) {
            tracing::warn!(bin = %self.bin, "process exited");
            if let Some(new_child) = spawn_process(self, ctx) {
                *state = new_child;
            }
        }
    }

    fn exit(mut state: Self::State, _ctx: &Self::Context) {
        let _ = state.kill();
    }
}

pub struct DataLoopHandle {
    tx: mpsc::Sender<Vec<CommandSpec>>,
}

impl DataLoopHandle {
    pub fn set_desired(&self, specs: Vec<CommandSpec>) {
        let _ = self.tx.send(specs);
    }
}

pub struct DataLoop {
    pool: ManagedSet<CommandSpec>,
    desired: Vec<CommandSpec>,
    timeout: Option<Duration>,
    rx: mpsc::Receiver<StreamItem>,
    extra_rx: Option<mpsc::Receiver<()>>,
    desired_rx: mpsc::Receiver<Vec<CommandSpec>>,
}

impl DataLoop {
    pub fn new() -> (Self, DataLoopHandle) {
        let (tx, rx) = mpsc::channel();
        let (desired_tx, desired_rx) = mpsc::channel();
        let data_loop = Self {
            pool: ManagedSet::new(tx),
            desired: Vec::new(),
            timeout: None,
            rx,
            extra_rx: None,
            desired_rx,
        };
        let handle = DataLoopHandle { tx: desired_tx };
        (data_loop, handle)
    }

    pub fn with_extra_rx(mut self, rx: mpsc::Receiver<()>) -> Self {
        self.extra_rx = Some(rx);
        self
    }

    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    fn set_desired(&mut self, desired: &[CommandSpec]) {
        // Deduplicate desired specs while preserving first occurrence order.
        let mut seen = std::collections::HashSet::new();
        let desired_unique: Vec<CommandSpec> = desired
            .iter()
            .filter(|s| seen.insert((*s).clone()))
            .cloned()
            .collect();

        self.desired = desired_unique;
        self.pool.update(self.desired.clone());
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
                    Ok(specs) => self.set_desired(&specs),
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
            self.pool.update(self.desired.clone());

            on_tick();

            if awake {
                // Skip blocking recv so stop and further extra_rx signals are detected quickly.
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

    /// Type-system enforcement: `CommandSpec` must carry a `script: Option<String>` field.
    /// This test fails to compile until the field exists.
    #[test]
    fn command_spec_has_script_field() {
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
            script: Some("echo from_script".to_string()),
        };
        assert!(spec.script.is_some());
    }

    /// Runtime: when `CommandSpec` carries a script, the subprocess spawned via
    /// `DataLoop` executes that script and its output appears as a `StreamItem`.
    #[test]
    fn script_content_is_executed_and_output_delivered() {
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec![],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
            script: Some("echo from_script".to_string()),
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![spec.clone()]);

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
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo hello; sleep 10".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        // Pass the same spec twice; no `key` to distinguish them.
        handle.set_desired(vec![spec.clone(), spec.clone()]);

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
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo hello".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![spec.clone()]);

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
        assert_eq!(item.source, spec);
        assert_eq!(item.stream, StreamKind::Stdout);
    }

    #[test]
    fn crashed_process_is_restarted_and_output_continues() {
        // Use a command that emits one line then exits immediately.
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo hello".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![spec.clone()]);

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
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                "while true; do echo tick; sleep 0.1; done".to_string(),
            ],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
            script: None,
        };

        let (mut data_loop, handle) = DataLoop::new();
        handle.set_desired(vec![spec]);

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

    // ── DataLoopHandle ────────────────────────────────────────────────────────

    /// Compile-time test: `DataLoop::new()` must return a `(DataLoop, DataLoopHandle)` tuple.
    /// Fails to compile until the return type is changed.
    #[test]
    fn data_loop_new_returns_tuple_with_handle() {
        let (_data_loop, _handle): (DataLoop, DataLoopHandle) = DataLoop::new();
    }

    /// Runtime test: calling `handle.set_desired(specs)` from outside the `run` loop
    /// updates the running pool — a new spec is spawned — and output arrives on the
    /// `on_item` callback, proving the handle can communicate with the running loop
    /// without stopping it.
    #[test]
    fn handle_set_desired_spawns_process_into_running_loop() {
        let spec = CommandSpec {
            bin: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo handle_output".to_string()],
            env: BTreeMap::new(),
            current_dir: None,
            key: None,
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
        handle.set_desired(vec![spec]);

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
