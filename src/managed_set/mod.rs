use std::collections::HashMap;
use std::hash::Hash;

pub trait Lifecycle {
    type Key: Hash + Eq + Clone;
    type State;
    type Context;
    fn key(&self) -> Self::Key;
    /// Returns `None` if initialization fails; the item is not added to the store.
    fn enter(self, ctx: &Self::Context) -> Option<Self::State>;
    fn update(self, state: &mut Self::State, ctx: &Self::Context);
    fn exit(state: Self::State, ctx: &Self::Context);
}

pub struct ManagedSet<T: Lifecycle> {
    store: HashMap<T::Key, T::State>,
}

impl<T: Lifecycle> ManagedSet<T> {
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
        }
    }

    pub fn update(&mut self, new_items: Vec<T>, ctx: &T::Context) {
        // Build new_map, deduplicating by key
        let mut new_map: HashMap<T::Key, T> = HashMap::new();
        for item in new_items {
            new_map.insert(item.key(), item);
        }

        // Exit: keys in store but not in new_map
        let exit_keys: Vec<T::Key> = self
            .store
            .keys()
            .filter(|k| !new_map.contains_key(*k))
            .cloned()
            .collect();
        for key in exit_keys {
            let state = self.store.remove(&key).unwrap();
            T::exit(state, ctx);
        }

        // Enter or update
        for (key, item) in new_map {
            if let Some(state) = self.store.get_mut(&key) {
                item.update(state, ctx);
            } else if let Some(state) = item.enter(ctx) {
                self.store.insert(key, state);
            }
        }
    }

    pub fn get(&self, key: &T::Key) -> Option<&T::State> {
        self.store.get(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&T::Key, &T::State)> {
        self.store.iter()
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

        fn key(&self) -> String {
            self.id.clone()
        }

        fn enter(self, ctx: &Self::Context) -> Option<Self::State> {
            ctx.lock().unwrap().push(format!("enter:{}", self.id));
            Some(self.value)
        }

        fn update(self, state: &mut Self::State, ctx: &Self::Context) {
            ctx.lock().unwrap().push(format!("update:{}", self.id));
            *state = self.value;
        }

        fn exit(state: Self::State, ctx: &Self::Context) {
            ctx.lock().unwrap().push(format!("exit:{}", state));
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
        ms.update(vec![TestSpec { id: "a".to_string(), value: 42 }], &ctx);
        assert!(calls(&ctx).contains(&"enter:a".to_string()));
        assert_eq!(ms.get(&"a".to_string()), Some(&42));
    }

    // Test 2: update removing an existing item calls exit with the old state
    #[test]
    fn removed_item_calls_exit_with_old_state() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.update(vec![TestSpec { id: "a".to_string(), value: 99 }], &ctx);
        ms.update(vec![], &ctx);
        assert!(calls(&ctx).contains(&"exit:99".to_string()));
    }

    // Test 3: update with an already-present key calls update (not enter)
    #[test]
    fn existing_item_calls_update_not_enter() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.update(vec![TestSpec { id: "a".to_string(), value: 1 }], &ctx);
        ms.update(vec![TestSpec { id: "a".to_string(), value: 2 }], &ctx);
        let log = calls(&ctx);
        // Only one enter call total
        assert_eq!(log.iter().filter(|c| *c == "enter:a").count(), 1);
        // At least one update call
        assert!(log.contains(&"update:a".to_string()));
    }

    // Test 4: update deduplicates: two items with the same key → only one enter call
    #[test]
    fn duplicate_keys_in_batch_only_one_enter() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.update(vec![
            TestSpec { id: "a".to_string(), value: 1 },
            TestSpec { id: "a".to_string(), value: 2 },
        ], &ctx);
        let log = calls(&ctx);
        assert_eq!(log.iter().filter(|c| *c == "enter:a").count(), 1);
    }

    // Test 5: get returns the current state after enter
    #[test]
    fn get_returns_state_after_enter() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.update(vec![TestSpec { id: "b".to_string(), value: 7 }], &ctx);
        assert_eq!(ms.get(&"b".to_string()), Some(&7));
    }

    // Test 6: get returns updated state after update
    #[test]
    fn get_returns_updated_state_after_update() {
        let ctx = make_ctx();
        let mut ms: ManagedSet<TestSpec> = ManagedSet::new();
        ms.update(vec![TestSpec { id: "c".to_string(), value: 10 }], &ctx);
        ms.update(vec![TestSpec { id: "c".to_string(), value: 20 }], &ctx);
        assert_eq!(ms.get(&"c".to_string()), Some(&20));
    }
}
