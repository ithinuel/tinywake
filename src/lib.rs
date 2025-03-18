#![cfg_attr(not(test), no_std)]

use core::{
    sync::atomic::Ordering::Relaxed,
    task::{RawWaker, RawWakerVTable},
};

use portable_atomic::AtomicUsize;

/// Behaves like an Arc expect it is does not deallocate when counter reaches 0.
struct MyWaker {
    /// Mask matching the task it maps to. This is only set on Waker creation.
    mask: usize,
    /// Pointer to the task map to be updated on wake. This is only set on Waker creation.
    map_ptr: *const AtomicUsize,
    /// Reference counter to that waker.
    counter: AtomicUsize,
}
impl MyWaker {
    fn new(mask: usize, map_ptr: *const AtomicUsize) -> Self {
        Self {
            mask,
            map_ptr,
            counter: AtomicUsize::new(0),
        }
    }
    fn to_waker(&self) -> core::task::Waker {
        self.counter.fetch_add(1, Relaxed);
        // SAFETY: RAW_WAKER_VTABLE upholds the contract defined in `RawWakerVTable`. `self` is
        // owned by the executor context and is guaranteed not to be moved or released until it
        // checked the counter is back to 0.
        unsafe { core::task::Waker::new(self as *const _ as *const (), &RAW_WAKER_VTABLE) }
    }
}

const RAW_WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

unsafe fn waker_clone(me: *const ()) -> RawWaker {
    // SAFETY: Only a reference to MyWaker is expected here.
    let waker = unsafe { &*(me as *const MyWaker) };
    waker.counter.fetch_add(1, Relaxed);
    RawWaker::new(me, &RAW_WAKER_VTABLE)
}

unsafe fn waker_wake(me: *const ()) {
    // SAFETY: Only a reference to MyWaker is expected here.
    unsafe {
        waker_wake_by_ref(me);
        waker_drop(me)
    }
}

unsafe fn waker_wake_by_ref(me: *const ()) {
    // SAFETY: Only a reference to MyWaker is expected here.
    let waker = unsafe { &*(me as *const MyWaker) };
    // SAFETY: The AtomicUsize is guaranteed to live longer than this waker.
    let map = unsafe { &*waker.map_ptr };
    map.fetch_or(waker.mask, Relaxed);
}

unsafe fn waker_drop(me: *const ()) {
    // SAFETY: Only a reference to MyWaker is expected here.
    let waker = unsafe { &*(me as *const MyWaker) };
    waker.counter.fetch_sub(1, Relaxed);
}

struct AssertLessThanSizeOfUsize<const N: usize>;
impl<const N: usize> AssertLessThanSizeOfUsize<N> {
    const OK: () = assert!(
        N <= (usize::BITS as usize),
        "N must be less than size_of::<usize>()"
    );
}

/// Runs multiple async tasks concurrently on a bare-metal executor.
///
/// This function polls the provided futures in a loop until all tasks are complete.
/// It uses a custom waker to track task readiness and efficiently handles task scheduling
/// without requiring dynamic memory allocation.
///
/// ### Parameters
///
/// - `tasks`: An array of mutable references to futures implementing `Future<Output = ()>`.
///   The number of tasks (`N`) must not exceed the bit width of `usize` (e.g., 32 on 32-bit systems).
///
/// ### Behaviour
///
/// - Tasks are polled in a round-robin fashion whenever they signal readiness.
/// - The function blocks execution until **all** tasks are completed.
/// - Uses the `cortex-m::asm::wfe()` instruction to enter low-power mode when no tasks are ready.
///   (requires the `cortex-m` feature to be enabled).
///
/// ### Safety
///
/// - This function assumes that the provided futures remain valid and are not moved during execution.
/// - It performs atomic operations to track task readiness and wake events.
///
/// ### Panics
///
/// - Panics if `N` exceeds the bit width of `usize`.
///
/// ### Example
///
/// ```rust
/// use tinywake::run_all;
/// use core::future::Future;
///
/// async fn task() {
///     // Your async code here
/// }
///
/// fn main() {
///     let mut t1 = task();
///     let mut t2 = task();
///     run_all([&mut t1, &mut t2]);
/// }
/// ```
pub fn run_all<const N: usize>(tasks: [&mut dyn Future<Output = ()>; N]) {
    let () = AssertLessThanSizeOfUsize::<N>::OK;

    let mut live_tasks = (1 << N) - 1;
    let ready_map = AtomicUsize::new(live_tasks);
    let mut task_list: heapless::Vec<(&mut dyn Future<Output = ()>, MyWaker), N> =
        heapless::Vec::new();
    for (i, t) in tasks.into_iter().enumerate() {
        // ignore error as cannot fail. The Vec is sized after the array we iterate over.
        let _ = task_list.push((t, MyWaker::new(1 << i, &ready_map as *const _)));
    }

    while live_tasks != 0 {
        let mut mask = ready_map.swap(0, Relaxed);
        if mask == 0 {
            // WFI may cause the core to enter sleep/deepsleep even if a task has been awaken
            // between the swap and the check.
            // WFE will only block if no event/interrupt occurred since its last use.
            #[cfg(feature = "cortex-m")]
            cortex_m::asm::wfe();
            #[cfg(not(feature = "cortex-m"))]
            core::hint::spin_loop();
            continue;
        }

        while mask != 0 {
            let task_mask = mask & (!mask + 1);
            mask ^= task_mask;
            let task_idx = task_mask.trailing_zeros() as usize;

            let task = &mut task_list[task_idx];
            let waker = task.1.to_waker();
            let mut context = core::task::Context::from_waker(&waker);
            // We own a reference to the future. We know it will not be moving until we return.
            let fut = unsafe { core::pin::Pin::new_unchecked(&mut *task.0) };
            if let core::task::Poll::Ready(()) = fut.poll(&mut context) {
                live_tasks ^= task_mask
            }
        }
    }

    assert!(task_list.iter().all(|v| v.1.counter.load(Relaxed) == 0));
}

#[cfg(test)]
mod test {
    /*
     * Some test cases in this module were co-developed with assistance from a large language model.
     */
    use core::task::{Context, Poll};
    use futures::task::AtomicWaker;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestFuture {
        polled: AtomicBool,
        completed: Arc<AtomicBool>,
        waker: Arc<AtomicWaker>,
    }

    impl TestFuture {
        fn new() -> Self {
            Default::default()
        }
        fn complete_after_millis(&self, ms: u64) {
            let cmplt = self.completed.clone();
            let waker = self.waker.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                cmplt.store(true, Ordering::Relaxed);
                waker.wake();
            });
        }
    }

    impl Future for TestFuture {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polled.store(true, Ordering::Relaxed);
            if self.completed.load(Ordering::Relaxed) {
                Poll::Ready(())
            } else {
                self.waker.register(cx.waker());
                Poll::Pending
            }
        }
    }

    #[test]
    fn one_task() {
        let mut f = std::future::ready(());
        super::run_all([&mut f]);
    }

    #[test]
    fn multiple_tasks() {
        let mut f1 = std::future::ready(());
        let mut f2 = std::future::ready(());
        super::run_all([&mut f1, &mut f2]);
    }

    #[test]
    fn task_async_completion() {
        let mut f = TestFuture::new();
        f.complete_after_millis(100);
        super::run_all([&mut f]);
        assert!(f.polled.load(Ordering::Relaxed));
    }

    #[test]
    fn no_task() {
        super::run_all::<0>([]);
    }

    #[test]
    fn task_concurrent_progress() {
        let mut task1 = TestFuture::new();
        let mut task2 = TestFuture::new();

        task1.complete_after_millis(100);
        task2.complete_after_millis(200);

        super::run_all([&mut task1, &mut task2]);

        assert!(task1.polled.load(Ordering::Relaxed));
        assert!(task2.polled.load(Ordering::Relaxed));
    }

    #[test]
    fn task_polled_exact_count() {
        struct PartialFuture {
            polled_count: AtomicUsize,
            complete_after: usize,
        }

        impl PartialFuture {
            fn new(complete_after: usize) -> Self {
                Self {
                    polled_count: AtomicUsize::new(0),
                    complete_after,
                }
            }
        }

        impl Future for PartialFuture {
            type Output = ();

            fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                cx.waker().wake_by_ref(); // wake the task immediately
                let count = self.polled_count.fetch_add(1, Ordering::Relaxed);
                if count >= self.complete_after {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }

        let mut task = PartialFuture::new(3);
        super::run_all([&mut task]);
        assert_eq!(task.polled_count.load(Ordering::Relaxed), 4);
    }
}
