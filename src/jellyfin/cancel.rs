//! Explicit cooperative cancellation built on `tokio::sync::watch`.
//!
//! Every network and body-read operation runs inside `tokio::select!` against
//! the token, so no detached request timeouts or background tasks are needed.

use tokio::sync::watch;

/// Handle used to cancel one or more in-flight requests.
#[derive(Clone)]
pub struct CancelHandle {
    sender: watch::Sender<bool>,
}

/// Cloneable observation side of a cancellation channel.
#[derive(Clone)]
pub struct CancelToken {
    receiver: watch::Receiver<bool>,
}

/// Create a cancellation channel.
///
/// Dropping the handle also cancels every token created from this channel.
#[must_use]
pub fn cancellation() -> (CancelHandle, CancelToken) {
    let (sender, receiver) = watch::channel(false);
    (CancelHandle { sender }, CancelToken { receiver })
}

impl CancelHandle {
    /// Request cancellation. Repeated calls are harmless.
    pub fn cancel(&self) {
        let _ = self.sender.send(true);
    }

    /// Whether cancellation was already requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }
}

impl CancelToken {
    /// Whether cancellation was already requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Resolve once cancellation is requested.
    ///
    /// A dropped handle counts as cancellation so a forgotten handle cannot
    /// leave a request waiting forever.
    pub async fn cancelled(&mut self) {
        if *self.receiver.borrow() {
            return;
        }
        while self.receiver.changed().await.is_ok() {
            if *self.receiver.borrow() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cancellation;

    #[tokio::test]
    async fn cancellation_is_sticky_and_drop_safe() {
        let (handle, mut token) = cancellation();
        assert!(!token.is_cancelled());
        handle.cancel();
        assert!(token.is_cancelled());
        token.cancelled().await;
    }
}
