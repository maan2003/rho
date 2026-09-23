//! Routes Nix evaluation effects to the observers that record them.
//!
//! Upstream devenv also turned Nix activities into its TUI activity tree here;
//! rho only needs the effects, so that part is gone.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::eval_op::{EvalOp, OpObserver};

pub struct NixLogBridge {
    /// Read on every effect (hundreds of thousands per evaluation), changed
    /// rarely, hence copy-on-write.
    observers: ArcSwap<Vec<Arc<dyn OpObserver>>>,
}

impl NixLogBridge {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            observers: ArcSwap::from_pointee(Vec::new()),
        })
    }

    /// Register an observer for every evaluation effect from now on.
    pub fn add_observer(&self, observer: Arc<dyn OpObserver>) {
        self.observers.rcu(|current| {
            let mut next = (**current).clone();
            next.push(Arc::clone(&observer));
            next
        });
    }

    /// Remove a previously-added observer by `Arc` identity.
    pub fn remove_observer(&self, observer: &Arc<dyn OpObserver>) {
        self.observers.rcu(|current| {
            current
                .iter()
                .filter(|candidate| !Arc::ptr_eq(candidate, observer))
                .cloned()
                .collect::<Vec<_>>()
        });
    }

    /// Clear all observers.
    pub fn clear_observers(&self) {
        self.observers.store(Arc::new(Vec::new()));
    }

    /// Handle one effect from Nix's eval-effect callback.
    pub fn process_eval_effect(&self, kind: &str, subject: &str, detail: Option<&str>) {
        let Some(op) = EvalOp::from_effect(kind, subject, detail) else {
            tracing::trace!(kind, subject, ?detail, "ignoring unknown eval effect");
            return;
        };
        let observers = self.observers.load();
        for observer in observers.iter() {
            observer.record(op.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Helper: create a mock observer that records ops in a shared Vec.
    struct MockObserver {
        ops: Mutex<Vec<EvalOp>>,
    }

    impl MockObserver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                ops: Mutex::new(Vec::new()),
            })
        }

        fn collected_ops(&self) -> Vec<EvalOp> {
            self.ops.lock().unwrap().clone()
        }
    }

    impl OpObserver for MockObserver {
        fn record(&self, op: EvalOp) {
            self.ops.lock().unwrap().push(op);
        }
    }

    #[test]
    fn test_add_observer_receives_dispatched_ops() {
        let bridge = NixLogBridge::new();
        let observer = MockObserver::new();
        bridge.add_observer(observer.clone());

        bridge.process_eval_effect("evaluated-file", "/tmp/default.nix", Some("uncached"));

        assert_eq!(observer.collected_ops().len(), 1);
        assert_eq!(
            observer.collected_ops()[0],
            EvalOp::EvaluatedFile {
                source: "/tmp/default.nix".into(),
                cached: false,
            }
        );
    }

    #[test]
    fn test_multiple_observers_all_receive_ops() {
        let bridge = NixLogBridge::new();
        let obs1 = MockObserver::new();
        let obs2 = MockObserver::new();
        bridge.add_observer(obs1.clone());
        bridge.add_observer(obs2.clone());

        bridge.process_eval_effect("evaluated-file", "/tmp/default.nix", Some("uncached"));

        assert_eq!(obs1.collected_ops().len(), 1);
        assert_eq!(obs2.collected_ops().len(), 1);
    }

    #[test]
    fn test_clear_observers_drops_all() {
        let bridge = NixLogBridge::new();
        let observer = MockObserver::new();
        bridge.add_observer(observer.clone());
        bridge.clear_observers();

        bridge.process_eval_effect("evaluated-file", "/tmp/default.nix", Some("uncached"));

        assert_eq!(observer.collected_ops().len(), 0);
    }
}
