use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::Hash;

pub mod reconcile;
pub use reconcile::{Reconcile, ReconcileErrors};

pub trait Lifecycle {
    /// Stable identity for this item. Forms a segment in the `KeyPath` used to address
    /// this item's logs and status in a supervisor tree.
    type Key: Hash + Eq + Clone + serde::Serialize + serde::de::DeserializeOwned;
    type State;
    type Context;
    type Output;
    type Error;

    /// Stable identity for this item within its parent's namespace.
    fn key(&self) -> Self::Key;

    /// Called once when this item first appears in the desired set. Returns the live
    /// state or an error; on `Err` the item is not added to the store.
    fn enter(self, ctx: &Self::Context, output: &Self::Output) -> Result<Self::State, Self::Error>;

    /// Called on every reconciliation cycle while the item remains present. Responsible
    /// for both synchronising this item's own state and triggering reconciliation of any
    /// child items it owns.
    fn reconcile_self(self, state: &mut Self::State, ctx: &Self::Context, output: &Self::Output) -> Result<(), Self::Error>;

    /// Called when this item leaves the desired set or after a failed `reconcile_self`.
    /// An `Err` return signals a zombie — cleanup did not complete cleanly.
    fn exit(state: Self::State, ctx: &Self::Context) -> Result<(), Self::Error>;
}

pub struct ManagedSet<T: Lifecycle> {
    store: HashMap<T::Key, T::State>,
}

impl<T: Lifecycle> Default for ManagedSet<T> {
    fn default() -> Self {
        Self { store: HashMap::new() }
    }
}

impl<T: Lifecycle + 'static> ManagedSet<T>
where
    T::Error: Debug,
{
    pub fn new() -> Self {
        Self::default()
    }

    fn dedup_by_key(items: impl IntoIterator<Item = T>) -> HashMap<T::Key, T> {
        let mut map = HashMap::new();
        for item in items {
            map.insert(item.key(), item);
        }
        map
    }

    fn exit_removed(&mut self, new_map: &HashMap<T::Key, T>, ctx: &T::Context, errors: &mut ReconcileErrors<T::Key, T::Error>) {
        let exit_keys: Vec<T::Key> = self.store.keys()
            .filter(|k| !new_map.contains_key(*k))
            .cloned()
            .collect();
        for key in exit_keys {
            let state = self.store.remove(&key).unwrap();
            if let Err(e) = T::exit(state, ctx) {
                errors.push((key, e));
            }
        }
    }

    fn update_existing(&mut self, new_map: &mut HashMap<T::Key, T>, ctx: &T::Context, output: &T::Output, errors: &mut ReconcileErrors<T::Key, T::Error>) {
        let update_keys: Vec<T::Key> = new_map.keys()
            .filter(|k| self.store.contains_key(*k))
            .cloned()
            .collect();
        for key in update_keys {
            let item = new_map.remove(&key).unwrap();
            let state = self.store.get_mut(&key).unwrap();
            if let Err(e) = item.reconcile_self(state, ctx, output) {
                let old_state = self.store.remove(&key).unwrap();
                if let Err(exit_e) = T::exit(old_state, ctx) {
                    errors.push((key.clone(), exit_e));
                }
                errors.push((key, e));
            }
        }
    }

    fn enter_new(&mut self, mut new_map: HashMap<T::Key, T>, ctx: &T::Context, output: &T::Output, errors: &mut ReconcileErrors<T::Key, T::Error>) {
        let enter_keys: Vec<T::Key> = new_map.keys()
            .filter(|k| !self.store.contains_key(*k))
            .cloned()
            .collect();
        for key in enter_keys {
            let item = new_map.remove(&key).unwrap();
            match item.enter(ctx, output) {
                Ok(state) => { self.store.insert(key, state); }
                Err(e) => { errors.push((key, e)); }
            }
        }
    }

    pub fn get(&self, key: &T::Key) -> Option<&T::State> {
        self.store.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&T::Key, &T::State)> {
        self.store.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&T::Key, &mut T::State)> {
        self.store.iter_mut()
    }

    pub fn get_mut(&mut self, key: &T::Key) -> Option<&mut T::State> {
        self.store.get_mut(key)
    }
}

impl<T: Lifecycle + 'static> reconcile::Reconcile<T> for ManagedSet<T>
where
    T::Error: Debug,
{
    fn reconcile(&mut self, desired: impl IntoIterator<Item = T>, ctx: &T::Context, output: &T::Output)
        -> ReconcileErrors<T::Key, T::Error>
    {
        let mut errors = ReconcileErrors::new();
        let mut new_map = Self::dedup_by_key(desired);
        self.exit_removed(&new_map, ctx, &mut errors);
        self.update_existing(&mut new_map, ctx, output, &mut errors);
        self.enter_new(new_map, ctx, output, &mut errors);
        errors
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TestSpec {
        id: String,
        value: i32,
    }

    impl Lifecycle for TestSpec {
        type Key = String;
        type State = i32;
        type Context = Arc<Mutex<Vec<String>>>;
        type Output = ();
        type Error = std::convert::Infallible;

        fn key(&self) -> String {
            self.id.clone()
        }

        fn enter(self, ctx: &Self::Context, _output: &()) -> Result<Self::State, Self::Error> {
            ctx.lock().unwrap().push(format!("enter:{}", self.id));
            Ok(self.value)
        }

        fn reconcile_self(self, state: &mut Self::State, ctx: &Self::Context, _output: &()) -> Result<(), Self::Error> {
            ctx.lock().unwrap().push(format!("reconcile_self:{}", self.id));
            *state = self.value;
            Ok(())
        }

        fn exit(state: Self::State, ctx: &Self::Context) -> Result<(), Self::Error> {
            ctx.lock().unwrap().push(format!("exit:{}", state));
            Ok(())
        }
    }

    fn make_ctx() -> Arc<Mutex<Vec<String>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn calls(ctx: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        ctx.lock().unwrap().clone()
    }

    // Test 1: update with a new item calls enter and stores the returned state
    #[test]
    fn new_item_calls_enter_and_stores_state() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "a".to_string(), value: 42 }], &ctx, &());
        assert!(calls(&ctx).contains(&"enter:a".to_string()));
        assert_eq!(ms.get(&"a".to_string()), Some(&42));
    }

    // Test 2: update removing an existing item calls exit with the old state
    #[test]
    fn removed_item_calls_exit_with_old_state() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "a".to_string(), value: 99 }], &ctx, &());
        ms.reconcile(vec![], &ctx, &());
        assert!(calls(&ctx).contains(&"exit:99".to_string()));
    }

    // Test 3: update with an already-present key calls update (not enter)
    #[test]
    fn existing_item_calls_update_not_enter() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "a".to_string(), value: 1 }], &ctx, &());
        ms.reconcile(vec![TestSpec { id: "a".to_string(), value: 2 }], &ctx, &());
        let log = calls(&ctx);
        // Only one enter call total
        assert_eq!(log.iter().filter(|c| *c == "enter:a").count(), 1);
        // At least one reconcile_self call
        assert!(log.contains(&"reconcile_self:a".to_string()));
    }

    // Test 4: update deduplicates: two items with the same key → only one enter call
    #[test]
    fn duplicate_keys_in_batch_only_one_enter() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![
            TestSpec { id: "a".to_string(), value: 1 },
            TestSpec { id: "a".to_string(), value: 2 },
        ], &ctx, &());
        let log = calls(&ctx);
        assert_eq!(log.iter().filter(|c| *c == "enter:a").count(), 1);
    }

    // Test 5: get returns the current state after enter
    #[test]
    fn get_returns_state_after_enter() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "b".to_string(), value: 7 }], &ctx, &());
        assert_eq!(ms.get(&"b".to_string()), Some(&7));
    }

    // Test 6: get returns updated state after update
    #[test]
    fn get_returns_updated_state_after_update() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "c".to_string(), value: 10 }], &ctx, &());
        ms.reconcile(vec![TestSpec { id: "c".to_string(), value: 20 }], &ctx, &());
        assert_eq!(ms.get(&"c".to_string()), Some(&20));
    }

    // Test 7 (Claim A): iter_mut yields (&Key, &mut State) pairs; mutations are
    // visible through subsequent get calls.
    #[test]
    fn iter_mut_yields_mutable_state_visible_via_get() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "d".to_string(), value: 5 }], &ctx, &());
        for (_k, v) in ms.iter_mut() {
            *v = 99;
        }
        assert_eq!(ms.get(&"d".to_string()), Some(&99));
    }

    // Test 8 (Claim B): get_mut returns a mutable reference; a mutation through it
    // is visible via the subsequent get call.
    #[test]
    fn get_mut_returns_mutable_reference_visible_via_get() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.reconcile(vec![TestSpec { id: "e".to_string(), value: 3 }], &ctx, &());
        if let Some(v) = ms.get_mut(&"e".to_string()) {
            *v = 77;
        }
        assert_eq!(ms.get(&"e".to_string()), Some(&77));
    }

    // ---------------------------------------------------------------------------
    // Cycle 1: enter returning Err → item not added to store, error returned
    // ---------------------------------------------------------------------------

    #[derive(Clone)]
    struct FallibleSpec {
        id: String,
        fail: bool,
    }

    #[derive(Debug, PartialEq)]
    struct FallibleError(String);

    impl Lifecycle for FallibleSpec {
        type Key = String;
        type State = String;
        type Context = ();
        type Output = ();
        type Error = FallibleError;

        fn key(&self) -> String {
            self.id.clone()
        }

        fn enter(self, _ctx: &(), _output: &()) -> Result<String, FallibleError> {
            if self.fail {
                Err(FallibleError(format!("enter failed for {}", self.id)))
            } else {
                Ok(format!("state:{}", self.id))
            }
        }

        fn reconcile_self(self, state: &mut String, _ctx: &(), _output: &()) -> Result<(), FallibleError> {
            *state = format!("updated:{}", self.id);
            Ok(())
        }

        fn exit(_state: String, _ctx: &()) -> Result<(), FallibleError> {
            Ok(())
        }
    }

    // Claim: when enter returns Err, the item is not added to the store and the
    // error is included in the Vec returned by ManagedSet::update.
    #[test]
    fn enter_err_not_added_to_store_error_returned() {
        let mut ms: ManagedSet<FallibleSpec> = ManagedSet::new();
        let errors = ms.reconcile(vec![FallibleSpec { id: "x".to_string(), fail: true }], &(), &());
        assert!(ms.get(&"x".to_string()).is_none(), "item must not be in store after enter Err");
        assert_eq!(errors.len(), 1, "one error must be returned");
        assert_eq!(errors[0].0, "x");
        assert_eq!(errors[0].1, FallibleError("enter failed for x".to_string()));
    }

    // Claim: when enter returns Ok, the item IS added to the store and no errors returned.
    #[test]
    fn enter_ok_adds_item_to_store_no_errors() {
        let mut ms: ManagedSet<FallibleSpec> = ManagedSet::new();
        let errors = ms.reconcile(vec![FallibleSpec { id: "y".to_string(), fail: false }], &(), &());
        assert_eq!(ms.get(&"y".to_string()), Some(&"state:y".to_string()));
        assert!(errors.is_empty(), "no errors when enter returns Ok");
    }

    // ---------------------------------------------------------------------------
    // Cycle 2: update returning Err → exit called, entry removed, error returned
    // ---------------------------------------------------------------------------

    #[derive(Clone)]
    struct UpdateFallibleSpec {
        id: String,
        fail_update: bool,
    }

    #[derive(Debug, PartialEq)]
    struct UpdateError(String);

    static EXIT_CALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    impl Lifecycle for UpdateFallibleSpec {
        type Key = String;
        type State = String;
        type Context = ();
        type Output = ();
        type Error = UpdateError;

        fn key(&self) -> String {
            self.id.clone()
        }

        fn enter(self, _ctx: &(), _output: &()) -> Result<String, UpdateError> {
            Ok(format!("state:{}", self.id))
        }

        fn reconcile_self(self, _state: &mut String, _ctx: &(), _output: &()) -> Result<(), UpdateError> {
            if self.fail_update {
                Err(UpdateError(format!("update failed for {}", self.id)))
            } else {
                Ok(())
            }
        }

        fn exit(_state: String, _ctx: &()) -> Result<(), UpdateError> {
            EXIT_CALLED.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    // Claim: when update returns Err, exit is called on the old state, the entry is
    // removed from the store, and the error is returned. Next call will use enter.
    #[test]
    fn update_err_exit_called_entry_removed_error_returned() {
        EXIT_CALLED.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut ms: ManagedSet<UpdateFallibleSpec> = ManagedSet::new();

        // First: enter succeeds
        let e1 = ms.reconcile(vec![UpdateFallibleSpec { id: "z".to_string(), fail_update: false }], &(), &());
        assert!(e1.is_empty());
        assert!(ms.get(&"z".to_string()).is_some(), "item should be in store after successful enter");

        // Second: update fails
        let errors = ms.reconcile(vec![UpdateFallibleSpec { id: "z".to_string(), fail_update: true }], &(), &());
        assert_eq!(errors.len(), 1, "one error must be returned on update failure");
        assert_eq!(errors[0].0, "z");
        assert_eq!(errors[0].1, UpdateError("update failed for z".to_string()));
        assert!(ms.get(&"z".to_string()).is_none(), "item must be removed from store after update Err");
        assert!(EXIT_CALLED.load(std::sync::atomic::Ordering::SeqCst), "exit must be called after update Err");

        // Third: next call uses enter (not update)
        EXIT_CALLED.store(false, std::sync::atomic::Ordering::SeqCst);
        let e3 = ms.reconcile(vec![UpdateFallibleSpec { id: "z".to_string(), fail_update: false }], &(), &());
        assert!(e3.is_empty(), "third call should succeed via enter");
        assert!(ms.get(&"z".to_string()).is_some(), "item should be re-entered on third call");
    }

    // ---------------------------------------------------------------------------
    // Output separation tests (RED — these require the new Lifecycle::Output type)
    // ---------------------------------------------------------------------------

    // A channel-based output type so we can observe what enter writes.
    use std::sync::mpsc;

    #[derive(Clone)]
    struct OutputSpec {
        id: String,
        value: i32,
    }

    impl Lifecycle for OutputSpec {
        type Key = String;
        type State = ();
        // Context is empty — all "live" communication goes through Output.
        type Context = ();
        // Output is the sender half of a channel.
        type Output = mpsc::Sender<String>;
        type Error = std::convert::Infallible;

        fn key(&self) -> String {
            self.id.clone()
        }

        // enter receives output and writes to it — this is the behavioral claim.
        fn enter(self, _ctx: &(), output: &mpsc::Sender<String>) -> Result<(), Self::Error> {
            output.send(format!("entered:{}", self.id)).unwrap();
            Ok(())
        }

        // reconcile_self also receives output.
        fn reconcile_self(self, _state: &mut (), _ctx: &(), output: &mpsc::Sender<String>) -> Result<(), Self::Error> {
            output.send(format!("reconciled:{}", self.id)).unwrap();
            Ok(())
        }

        // exit does NOT receive output — cleanup doesn't write to it.
        fn exit(_state: (), _ctx: &()) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    // Claim A: enter receives `output` and can write to it; Context is separate (here `()`).
    #[test]
    fn enter_receives_output_and_can_write_to_it() {
        let (tx, rx) = mpsc::channel::<String>();
        let mut ms: ManagedSet<OutputSpec> = ManagedSet::new();
        ms.reconcile(
            vec![OutputSpec { id: "o1".to_string(), value: 1 }],
            &(),
            &tx,
        );
        drop(tx); // close sender so recv() terminates
        let msgs: Vec<String> = rx.try_iter().collect();
        assert!(
            msgs.contains(&"entered:o1".to_string()),
            "enter must write to output; got: {:?}",
            msgs
        );
    }

    // Claim B: exit does NOT receive output — it only takes ctx (&()).
    // This is a compile-time claim. We verify it by confirming OutputSpec compiles
    // with an exit signature that has NO output parameter, proving the trait
    // requires exactly `fn exit(state, ctx)` and not `fn exit(state, ctx, output)`.
    //
    // The test body just exercises the exit path to confirm it runs without output.
    #[test]
    fn exit_does_not_receive_output() {
        let (tx, rx) = mpsc::channel::<String>();
        let mut ms: ManagedSet<OutputSpec> = ManagedSet::new();
        // Enter the item.
        ms.reconcile(vec![OutputSpec { id: "o2".to_string(), value: 2 }], &(), &tx);
        // Remove it — triggers exit, which has no output parameter.
        ms.reconcile(vec![], &(), &tx);
        drop(tx);
        // We only expect "entered:o2" — exit must NOT have sent anything to output.
        let msgs: Vec<String> = rx.try_iter().collect();
        assert!(
            !msgs.iter().any(|m| m.starts_with("exited:")),
            "exit must not write to output; got: {:?}",
            msgs
        );
    }
}
