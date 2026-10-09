//! Build a large future on the heap, outside the poll frame that awaits it.
//!
//! **Why.** An unpolled future is as large as its whole state machine: the
//! state of every future it awaits, nested, is part of it. In a build without
//! optimizations every `async fn` builds the future of each call it awaits in
//! a stack slot of its own poll frame before it moves that future into its
//! state, and those slots do not share space. A poll frame therefore grows
//! with the size of every future it awaits, and the frames of a chain of
//! awaits add up while the innermost future runs. The hosted run path (the
//! host's `PreparedRun::run`, then the orchestrator's `start`, its plan
//! review, `run`, `run_loop` and `on_finished`) measured 1.83 MB of stack on
//! the thread that polled it, in a debug build on macOS: 359 KB for one frame,
//! 242 KB and 203 KB for two more. Rust's test threads and tokio's worker
//! threads get 2 MiB, and a Windows build overflowed it.
//!
//! **What [`on_heap`] does.** The caller passes a closure that makes the
//! future; `on_heap` calls it in its own frame, moves the future into a box
//! and returns the box. The caller's poll frame and its state then hold one
//! pointer instead of the future, and `on_heap`'s frame is gone before the
//! future is first polled, so the slot it needed is not on the stack while
//! the work runs. `Box::pin(call())` would not do this: the argument is still
//! built in the caller's frame first.
//!
//! **Cost.** One allocation per call, on calls that each run for at least a
//! model request or a process. Not for a hot loop.

use std::future::Future;
use std::pin::Pin;

/// The future `make` returns, built in this function's frame and moved to
/// the heap (the module header says why).
///
/// Never inlined: inlined into the caller, the future would be built in the
/// caller's frame again, which is what this exists to avoid.
#[inline(never)]
pub fn on_heap<F, Fut>(make: F) -> Pin<Box<Fut>>
where
    F: FnOnce() -> Fut,
    Fut: Future,
{
    Box::pin(make())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn big() -> usize {
        let buf = [7u8; 64 * 1024];
        // Held across an await, so it is part of the future's state.
        std::future::ready(()).await;
        buf.iter().map(|b| usize::from(*b)).sum()
    }

    async fn awaits_inline() -> usize {
        big().await
    }

    async fn awaits_on_heap() -> usize {
        on_heap(big).await
    }

    /// The caller's future no longer holds the callee's state: awaited
    /// inline, a 64 KiB future makes its caller at least that large; through
    /// `on_heap`, the caller holds a pointer. The result is the same.
    #[test]
    fn the_callers_future_holds_a_pointer_not_the_callees_state() {
        assert!(std::mem::size_of_val(&awaits_inline()) >= 64 * 1024);
        assert!(std::mem::size_of_val(&awaits_on_heap()) < 1024);
        let run = |f: Pin<&mut dyn Future<Output = usize>>| {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            match f.poll(&mut cx) {
                std::task::Poll::Ready(v) => v,
                std::task::Poll::Pending => panic!("nothing here waits"),
            }
        };
        assert_eq!(run(Box::pin(awaits_on_heap()).as_mut()), 7 * 64 * 1024);
        assert_eq!(run(Box::pin(awaits_inline()).as_mut()), 7 * 64 * 1024);
    }
}
