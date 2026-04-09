use std::time::Duration;

use costae::spawn_module;

#[test]
fn spawn_module_receives_stdout_line_from_script() {
    let (rx, _child) = spawn_module("/usr/bin/bash", Some("echo hello"));
    let line = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(line, "hello");
}

#[test]
fn spawn_module_receives_multiple_lines() {
    let (rx, _child) = spawn_module("/usr/bin/bash", Some("echo first\necho second"));
    let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first, "first");
    assert_eq!(second, "second");
}

#[test]
fn spawn_module_works_without_script() {
    let (rx, _child) = spawn_module("/bin/echo", None);
    let line = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(line, "");
}

#[test]
fn killing_child_stops_receiver() {
    let (rx, mut child) = spawn_module("/usr/bin/bash", Some("while true; do echo tick; sleep 1; done"));
    rx.recv_timeout(Duration::from_secs(2)).unwrap(); // first line arrives
    child.kill().unwrap();
    child.wait().unwrap();
    // after kill, receiver should close (disconnect) eventually
    let result = rx.recv_timeout(Duration::from_secs(2));
    assert!(result.is_err());
}
