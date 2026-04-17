use std::collections::BTreeMap;
use std::io::BufRead;
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

pub struct DataLoop {
    pool: ManagedSet<CommandSpec>,
    desired: Vec<CommandSpec>,
    timeout: Option<Duration>,
    rx: mpsc::Receiver<StreamItem>,
}

impl Default for DataLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl DataLoop {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            pool: ManagedSet::new(tx),
            desired: Vec::new(),
            timeout: None,
            rx,
        }
    }

    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn set_desired(&mut self, desired: &[CommandSpec]) {
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

    pub fn run(&mut self, stop: Arc<AtomicBool>, mut handler: impl FnMut(StreamItem)) {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // Reconcile: enter new, exit removed, update existing (restarts crashed processes).
            self.pool.update(self.desired.clone());

            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(item) => handler(item),
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
        };

        let mut data_loop = DataLoop::new();
        // Pass the same spec twice; no `key` to distinguish them.
        data_loop.set_desired(&[spec.clone(), spec.clone()]);

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
            });
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
        };

        let mut data_loop = DataLoop::new();
        data_loop.set_desired(&[spec.clone()]);

        let items: Arc<Mutex<Vec<StreamItem>>> = Arc::new(Mutex::new(Vec::new()));
        let items_clone = Arc::clone(&items);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        data_loop.run(stop_for_run, |item| {
            items_clone.lock().unwrap().push(item);
            stop.store(true, Ordering::Relaxed);
        });

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
        };

        let mut data_loop = DataLoop::new();
        data_loop.set_desired(&[spec.clone()]);

        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_for_run = Arc::clone(&collected);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            data_loop.run(stop_for_run, |item| {
                collected_for_run.lock().unwrap().push(item.line);
            });
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
        let _ = handle.join();

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
        };

        let mut data_loop = DataLoop::new();
        data_loop.set_desired(&[spec]);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_run = Arc::clone(&stop);
        let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let collected_for_run = Arc::clone(&collected);

        let handle = thread::spawn(move || {
            data_loop.run(stop_for_run, |item| {
                collected_for_run.lock().unwrap().push(item.line);
            });
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
        let joined = handle.join();
        assert!(
            joined.is_ok(),
            "run() thread panicked or did not stop after cancellation token was set"
        );
    }
}
