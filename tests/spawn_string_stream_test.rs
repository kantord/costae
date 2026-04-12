use std::sync::mpsc;
use std::time::Duration;

use costae::spawn_bi_stream;
use costae::spawn_string_stream;

#[test]
fn spawn_string_stream_delivers_line_as_triple() {
    let (tx, rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::sync_channel(1);

    let _child = spawn_string_stream("sh", Some("echo hello"), tx, wake_tx);

    let (bin, script, line) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(bin, "sh");
    assert_eq!(script, Some("echo hello".to_string()));
    assert_eq!(line, "hello");

    // wake_tx must have been signalled after the line was sent
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn spawn_string_stream_signals_wake_tx_after_each_line() {
    let (tx, rx) = mpsc::channel();
    let (wake_tx, wake_rx) = mpsc::sync_channel(4);

    let _child =
        spawn_string_stream("sh", Some("echo first\necho second"), tx, wake_tx);

    // First line + wake signal
    let (_, _, line1) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(line1, "first");
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    // Second line + wake signal
    let (_, _, line2) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(line2, "second");
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn spawn_bi_stream_script_field_is_none() {
    let (tx, rx) = mpsc::channel();
    let (wake_tx, _wake_rx) = mpsc::sync_channel(1);
    // `echo` with no arguments prints a blank line immediately and exits
    let _bi = spawn_bi_stream("echo", &serde_json::json!(null), tx, wake_tx);
    let (bin, script, line) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(bin, "echo");
    assert_eq!(script, None, "spawn_bi_stream must forward script=None, not Some(...)");
    assert_eq!(line, "");
}
