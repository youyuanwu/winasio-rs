use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

/// Shared state between the future and the waiting thread
#[derive(Debug)]
struct SharedState {
    /// Whether or not the sleep time has elapsed
    completed: bool,

    /// The waker for the task that `TimerFuture` is running on.
    /// The thread can use this after setting `completed = true` to tell
    /// `TimerFuture`'s task to wake up, see that `completed = true`, and
    /// move forward.
    waker: Option<Waker>,
}

// rust async await object
pub struct AsyncWaitObject {
    shared_state: Arc<Mutex<SharedState>>,
}

pub struct AwaitableToken {
    shared_state: Arc<Mutex<SharedState>>,
}

impl Default for AsyncWaitObject {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncWaitObject {
    pub fn new() -> AsyncWaitObject {
        AsyncWaitObject {
            shared_state: Arc::new(Mutex::new(SharedState {
                completed: false,
                waker: None,
            })),
        }
    }

    // notify work is complete
    pub fn wake(&self) {
        let mut shared_state = self.shared_state.lock().unwrap();
        // Signal that the timer has completed and wake up the last
        // task on which the future was polled, if one exists.
        shared_state.completed = true;
        if let Some(waker) = shared_state.waker.take() {
            waker.wake()
        }
    }

    // reset state to reuse.
    pub fn reset(&mut self) {
        self.shared_state = Arc::new(Mutex::new(SharedState {
            completed: false,
            waker: None,
        }));
    }

    // make ctx unchanged when doing wait
    pub fn get_await_token(&self) -> AwaitableToken {
        AwaitableToken {
            shared_state: self.shared_state.clone(),
        }
    }
}

impl Future for AwaitableToken {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Look at the shared state to see if the timer has already completed.
        let mut shared_state = self.shared_state.lock().unwrap();
        if shared_state.completed {
            Poll::Ready(())
        } else {
            // Set waker so that the thread can wake up the current task
            // when the timer has completed, ensuring that the future is polled
            // again and sees that `completed = true`.
            //
            // It's tempting to do this once rather than repeatedly cloning
            // the waker each time. However, the `TimerFuture` can move between
            // tasks on the executor, which could cause a stale waker pointing
            // to the wrong task, preventing `TimerFuture` from waking up
            // correctly.
            //
            // N.B. it's possible to check for this using the `Waker::will_wake`
            // function, but we omit that here to keep things simple.
            shared_state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
