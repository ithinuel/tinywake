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
        N <= (size_of::<usize>() * 8),
        "N must be less than size_of::<usize>()"
    );
}
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
            #[cfg(not(test))]
            cortex_m::asm::wfe();
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
    #[test]
    fn ready() {
        let mut f = std::future::ready(());
        super::run_all([&mut f]);
    }
}
