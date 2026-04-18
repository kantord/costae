use std::collections::BTreeMap;
use std::io::{BufRead, Seek, SeekFrom, Write as IoWrite};
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;

use crate::managed_set::Lifecycle;

use super::{StreamItem, StreamKind};

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

pub struct ProcessState {
    pub child: std::process::Child,
    pub event_tx: mpsc::Sender<serde_json::Value>,
    pub last_sent_props: Option<serde_json::Value>,
}

pub(super) fn spawn_process(spec: ProcessSource, tx: &mpsc::Sender<StreamItem>) -> Option<ProcessState> {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use crate::managed_set::Lifecycle;
    use super::{ProcessIdentity, ProcessSource};

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

    #[test]
    fn process_identity_has_bin_and_key_fields() {
        let id = ProcessIdentity {
            bin: "mybin".to_string(),
            key: "mykey".to_string(),
        };
        assert_eq!(id.bin, "mybin");
        assert_eq!(id.key, "mykey");
    }

    #[test]
    fn process_identity_derives_hash_eq_partialeq_clone() {
        use std::collections::HashSet;
        let a = ProcessIdentity { bin: "bin".to_string(), key: "k".to_string() };
        let b = a.clone();
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a);
        assert!(!set.insert(b));
    }

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
        let returned: ProcessIdentity = spec.key();
        assert_eq!(returned, id);
    }
}
