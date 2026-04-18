use super::{Lifecycle, ReconcileErrors};

/// A reconciler that can apply a desired state from a `Vec<T>` to a managed
/// store, producing a list of per-key errors for items that failed to enter
/// or update.
///
/// This trait makes the reconciliation algorithm swappable: callers can
/// program against `impl Reconcile<T>` rather than `ManagedSet<T>` directly.
pub trait Reconcile<T: Lifecycle> {
    fn reconcile(&mut self, desired: impl IntoIterator<Item = T>, ctx: &T::Context)
        -> ReconcileErrors<T::Key, T::Error>;
}

#[cfg(test)]
mod tests {
    mod fixtures {
        use crate::managed_set::reconcile::{Reconcile, ReconcileErrors};
        use crate::managed_set::Lifecycle;
        use std::convert::Infallible;
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        pub struct Item {
            pub id: &'static str,
            pub value: i32,
        }

        pub type Ctx = Arc<Mutex<Vec<String>>>;

        impl Lifecycle for Item {
            type Key = &'static str;
            type State = i32;
            type Context = Ctx;
            type Error = Infallible;

            fn key(&self) -> &'static str { self.id }

            fn enter(self, ctx: &Ctx) -> Result<i32, Infallible> {
                ctx.lock().unwrap().push(format!("enter:{}", self.id));
                Ok(self.value)
            }

            fn update(self, state: &mut i32, ctx: &Ctx) -> Result<(), Infallible> {
                ctx.lock().unwrap().push(format!("update:{}", self.id));
                *state = self.value;
                Ok(())
            }

            fn exit(_state: i32, ctx: &Ctx) {
                ctx.lock().unwrap().push("exit".to_string());
            }
        }

        pub fn make_ctx() -> Ctx {
            Arc::new(Mutex::new(Vec::new()))
        }

        pub fn log(ctx: &Ctx) -> Vec<String> {
            ctx.lock().unwrap().clone()
        }

        pub struct RecordingReconciler {
            pub calls: Vec<Vec<&'static str>>,
        }

        impl RecordingReconciler {
            pub fn new() -> Self { Self { calls: Vec::new() } }
        }

        impl Reconcile<Item> for RecordingReconciler {
            fn reconcile(&mut self, desired: impl IntoIterator<Item = Item>, _ctx: &Ctx)
                -> ReconcileErrors<&'static str, Infallible>
            {
                self.calls.push(desired.into_iter().map(|i| i.id).collect());
                vec![]
            }
        }

        pub fn drive<R: Reconcile<Item>>(
            reconciler: &mut R,
            items: Vec<Item>,
            ctx: &Ctx,
        ) -> ReconcileErrors<&'static str, Infallible> {
            reconciler.reconcile(items, ctx)
        }
    }

    // Claim 1: any type implementing Reconcile<T> can be used where
    // impl Reconcile<Item> is expected — ManagedSet or a hand-written mock.
    mod trait_usability {
        use super::fixtures::{make_ctx, drive, Item, RecordingReconciler};
        use crate::managed_set::ManagedSet;

        #[test]
        fn accepts_managed_set() {
            let ctx = make_ctx();
            let mut ms: ManagedSet<Item> = ManagedSet::new();
            assert!(drive(&mut ms, vec![Item { id: "a", value: 1 }], &ctx).is_empty());
        }

        #[test]
        fn accepts_mock_reconciler() {
            let ctx = make_ctx();
            let mut mock = RecordingReconciler::new();
            drive(&mut mock, vec![Item { id: "b", value: 2 }], &ctx);
            assert_eq!(mock.calls, vec![vec!["b"]]);
        }
    }

    // Claim 2: ManagedSet fires the correct Lifecycle callback for each scenario
    // when called through the Reconcile trait.
    mod managed_set_via_trait {
        use super::fixtures::{make_ctx, log, Item};
        use crate::managed_set::{ManagedSet, Reconcile};

        fn check<R: Reconcile<Item>>(reconciler: &mut R, setup: Vec<Item>, action: Vec<Item>, expected_log_entry: &str) {
            let ctx = make_ctx();
            reconciler.reconcile(setup, &ctx);
            reconciler.reconcile(action, &ctx);
            assert!(
                log(&ctx).iter().any(|e| e == expected_log_entry),
                "expected {:?} in log, got {:?}", expected_log_entry, log(&ctx)
            );
        }

        #[test]
        fn calls_enter_for_new_item() {
            check(&mut ManagedSet::new(), vec![], vec![Item { id: "a", value: 1 }], "enter:a");
        }

        #[test]
        fn calls_update_for_existing_item() {
            check(
                &mut ManagedSet::new(),
                vec![Item { id: "b", value: 1 }],
                vec![Item { id: "b", value: 2 }],
                "update:b",
            );
        }

        #[test]
        fn calls_exit_for_removed_item() {
            check(&mut ManagedSet::new(), vec![Item { id: "c", value: 5 }], vec![], "exit");
        }
    }

    // Claim 3: a test-double RecordingReconciler records calls without managing
    // any real state, and can be injected wherever impl Reconcile<Item> is expected.
    mod mock_reconciler {
        use super::fixtures::{make_ctx, log, drive, Item, RecordingReconciler};

        #[test]
        fn records_calls_without_managing_state() {
            let ctx = make_ctx();
            let mut mock = RecordingReconciler::new();

            drive(&mut mock, vec![Item { id: "x", value: 99 }], &ctx);
            drive(&mut mock, vec![Item { id: "y", value: 1 }, Item { id: "z", value: 2 }], &ctx);

            assert_eq!(mock.calls.len(), 2);
            assert_eq!(mock.calls[0], vec!["x"]);
            assert!(mock.calls[1].contains(&"y"));
            assert!(mock.calls[1].contains(&"z"));
            assert!(log(&ctx).is_empty(), "mock must not invoke lifecycle callbacks");
        }
    }
}
