use std::future::Future;
use std::pin::Pin;

use tokio::sync::OnceCell;

type InitFuture<T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send>>;

/// A concurrently initialized async value whose failed initialization may be
/// retried. Dropping a caller while initialization is in flight also leaves
/// the value uninitialized for the next caller.
pub(crate) struct Lazy<T> {
    value: OnceCell<T>,
    initialize: Box<dyn Fn() -> InitFuture<T> + Send + Sync>,
}

impl<T> Lazy<T> {
    pub(crate) fn new<F, Fut>(initialize: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        Self {
            value: OnceCell::new(),
            initialize: Box::new(move || Box::pin(initialize())),
        }
    }

    pub(crate) fn ready(value: T) -> Self {
        Self {
            value: OnceCell::new_with(Some(value)),
            initialize: Box::new(|| Box::pin(async { unreachable!("value is initialized") })),
        }
    }

    pub(crate) async fn get(&self) -> anyhow::Result<&T> {
        self.value.get_or_try_init(|| (self.initialize)()).await
    }

    pub(crate) fn get_if_ready(&self) -> Option<&T> {
        self.value.get()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Lazy;

    #[tokio::test]
    async fn retries_failed_initialization() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let lazy = Lazy::new({
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        anyhow::bail!("not yet")
                    }
                    Ok(42)
                }
            }
        });

        assert!(lazy.get().await.is_err());
        assert_eq!(*lazy.get().await.unwrap(), 42);
        assert_eq!(*lazy.get().await.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ready_value_does_not_initialize() {
        assert_eq!(*Lazy::ready(42).get().await.unwrap(), 42);
    }
}
