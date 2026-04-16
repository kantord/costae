use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

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

#[derive(Debug, PartialEq)]
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

pub struct DataLoop {
    pool: HashMap<CommandSpec, std::process::Child>,
    desired: Vec<CommandSpec>,
    timeout: Option<Duration>,
    tx: mpsc::Sender<StreamItem>,
    rx: mpsc::Receiver<StreamItem>,
}

impl DataLoop {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        DataLoop {
            pool: HashMap::new(),
            desired: Vec::new(),
            timeout: None,
            tx,
            rx,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
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

        // Save desired set for reconciliation in run().
        self.desired = desired_unique.clone();

        // Kill and remove specs no longer desired.
        let to_remove: Vec<CommandSpec> = self
            .pool
            .keys()
            .filter(|k| !seen.contains(*k))
            .cloned()
            .collect();
        for spec in to_remove {
            if let Some(mut child) = self.pool.remove(&spec) {
                let _ = child.kill();
            }
        }

        // Spawn specs that are desired but not yet in the pool.
        self.reconcile_pool();
    }

    /// Spawn any desired specs that are not currently in the pool.
    fn reconcile_pool(&mut self) {
        let desired = self.desired.clone();
        for spec in desired {
            if self.pool.contains_key(&spec) {
                continue;
            }

            let spec_clone = spec.clone();
            let tx = self.tx.clone();

            let mut cmd = std::process::Command::new(&spec_clone.bin);
            cmd.args(&spec_clone.args);
            for (k, v) in &spec_clone.env {
                cmd.env(k, v);
            }
            if let Some(ref dir) = spec_clone.current_dir {
                cmd.current_dir(dir);
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[data_loop] failed to spawn {}: {}", spec_clone.bin, e);
                    continue;
                }
            };

            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    self.pool.insert(spec, child);
                    continue;
                }
            };

            let spec_for_stdout = spec_clone.clone();
            let tx_stdout = tx.clone();
            thread::spawn(move || {
                let reader = std::io::BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            let item = StreamItem {
                                source: spec_for_stdout.clone(),
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

            // Spawn stderr reader thread — lines are logged to our own stderr.
            if let Some(stderr) = child.stderr.take() {
                let bin_name = spec_clone.bin.clone();
                thread::spawn(move || {
                    let reader = std::io::BufReader::new(stderr);
                    for line in reader.lines() {
                        match line {
                            Ok(l) => eprintln!("[module {}] {}", bin_name, l),
                            Err(_) => break,
                        }
                    }
                });
            }

            self.pool.insert(spec, child);
        }
    }

    pub fn run(&mut self, stop: Arc<AtomicBool>, mut handler: impl FnMut(StreamItem)) {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // Reconcile: check for exited processes and respawn them.
            let exited: Vec<CommandSpec> = self
                .pool
                .iter_mut()
                .filter_map(|(spec, child)| {
                    match child.try_wait() {
                        Ok(Some(_)) => Some(spec.clone()),
                        _ => None,
                    }
                })
                .collect();
            for spec in exited {
                eprintln!("[data_loop] process exited: {}", spec.bin);
                self.pool.remove(&spec);
            }
            self.reconcile_pool();

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
