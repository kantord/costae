use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use zbus::interface;
use zbus::zvariant::OwnedValue;

// ── Notification model ────────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
struct Notification {
    id: u32,
    app_name: String,
    summary: String,
    body: String,
    /// 0=low 1=normal 2=critical
    urgency: u8,
}

// ── Events flowing into the main state loop ───────────────────────────────────

enum Event {
    Add(Notification, i32 /* expire_timeout from spec */),
    Remove(u32),
}

// ── D-Bus notification server ─────────────────────────────────────────────────

struct NotifyServer {
    tx: mpsc::UnboundedSender<Event>,
    next_id: AtomicU32,
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotifyServer {
    async fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id == 0 {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        } else {
            replaces_id
        };

        let urgency = hints
            .get("urgency")
            .and_then(|v| u8::try_from(v.clone()).ok())
            .unwrap_or(1);

        let _ = self.tx.send(Event::Add(
            Notification {
                id,
                app_name: app_name.to_string(),
                summary: summary.to_string(),
                body: body.to_string(),
                urgency,
            },
            expire_timeout,
        ));

        id
    }

    async fn close_notification(&self, id: u32) {
        let _ = self.tx.send(Event::Remove(id));
    }

    async fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_string()]
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "costae-notify".to_string(),
            "costae".to_string(),
            "0.1.0".to_string(),
            "1.3".to_string(),
        )
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn emit(notifications: &[Notification]) {
    if let Ok(json) = serde_json::to_string(&serde_json::json!({ "notifications": notifications })) {
        println!("{json}");
    }
}

fn expire_ms(timeout: i32) -> Option<u64> {
    match timeout {
        0 => None,            // never
        -1 => Some(5_000),    // server default: 5 s
        ms => Some(ms as u64),
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Drain stdin so costae doesn't block when it writes the init event; we
    // don't need any of its fields for a notification daemon.
    tokio::task::spawn_blocking(|| {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        // Read exactly one line (the init event) and discard it.
        let _ = lines.next();
    });

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();

    let server = NotifyServer {
        tx: event_tx.clone(),
        next_id: AtomicU32::new(1),
    };

    let _conn = zbus::connection::Builder::session()
        .expect("session bus unavailable")
        .name("org.freedesktop.Notifications")
        .expect("could not claim org.freedesktop.Notifications — is another daemon running?")
        .serve_at("/org/freedesktop/Notifications", server)
        .expect("serve_at failed")
        .build()
        .await
        .expect("D-Bus connection failed");

    // Emit empty list immediately so costae has an initial value.
    emit(&[]);

    let mut notifications: Vec<Notification> = Vec::new();

    while let Some(event) = event_rx.recv().await {
        match event {
            Event::Add(n, timeout) => {
                let id = n.id;
                // Replace if same id, otherwise append.
                if let Some(pos) = notifications.iter().position(|x| x.id == id) {
                    notifications[pos] = n;
                } else {
                    notifications.push(n);
                }
                emit(&notifications);

                // Schedule auto-removal.
                if let Some(ms) = expire_ms(timeout) {
                    let tx = event_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        let _ = tx.send(Event::Remove(id));
                    });
                }
            }
            Event::Remove(id) => {
                let before = notifications.len();
                notifications.retain(|n| n.id != id);
                if notifications.len() != before {
                    emit(&notifications);
                }
            }
        }
    }
}
