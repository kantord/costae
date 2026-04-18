use std::sync::mpsc;

use crate::managed_set::Lifecycle;

use super::{StreamItem, StreamKind};

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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;
    use crate::managed_set::Lifecycle;
    use super::InternalSource;
    use super::super::StreamItem;

    #[test]
    fn internal_source_enter_emits_stream_item_with_correct_key_and_line() {
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
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "enter must emit exactly one StreamItem"
        );
    }

    #[test]
    fn internal_source_update_emits_stream_item_when_value_changes() {
        let (tx, rx) = mpsc::channel::<StreamItem>();
        let source_v1 = InternalSource {
            key: "upd-key".to_string(),
            value: serde_json::json!(1),
        };
        let mut state = source_v1.enter(&tx).expect("enter must succeed");
        let _ = rx.recv_timeout(Duration::from_millis(200))
            .expect("enter must emit an item");

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

    #[test]
    fn internal_source_update_does_not_emit_when_value_unchanged() {
        let (tx, rx) = mpsc::channel::<StreamItem>();
        let source_v1 = InternalSource {
            key: "dedup-key".to_string(),
            value: serde_json::json!({"x": 7}),
        };
        let mut state = source_v1.enter(&tx).expect("enter must succeed");
        let _ = rx.recv_timeout(Duration::from_millis(200))
            .expect("enter must emit an item");

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

}
