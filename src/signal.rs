use branches::likely;
use core::{cell::UnsafeCell, fmt::Debug, sync::atomic::Ordering};
use core::{mem::MaybeUninit, task::Waker};
use std::{cell::Cell, time::Duration};

const UNINIT: usize = 0;
const LOCKED: usize = UNINIT + 1;
const LOCKED_STARVATION: usize = UNINIT + 2;
const TERMINATED: usize = !0 - 1;
const UNLOCKED: usize = !0;
const DONE: usize = usize::MAX / 2;

#[repr(C)]
pub(crate) struct DynamicSignal<T> {
    ptr: *const (),
    _marker: core::marker::PhantomData<T>,
}

enum PointerResult<T> {

    Async(*const AsyncSignal<T>),
}

impl<T> DynamicSignal<T> {

    pub(crate) fn new_async(ptr: *const AsyncSignal<T>) -> Self {
        Self {
            ptr: ptr.cast(),
            _marker: core::marker::PhantomData,
        }
    }
    pub(crate) fn eq_ptr(&self, ptr: *const ()) -> bool {
        core::ptr::eq(self.ptr, ptr)
    }
    #[inline(always)]
    fn resolve(&self) -> PointerResult<T> {
        PointerResult::Async(self.ptr.cast())
    }
    #[inline(always)]
    pub(crate) unsafe fn send(self, data: T) {
        match self.resolve() {
            PointerResult::Async(ptr) => unsafe {
                AsyncSignal::write_data(ptr, data);
            },
        }
    }
    #[inline(always)]
    pub(crate) unsafe fn recv(self) -> T {
        match self.resolve() {
            PointerResult::Async(ptr) => unsafe { AsyncSignal::read_data(ptr) },
        }
    }
    #[inline(always)]
    pub(crate) unsafe fn terminate(&self) {
        match self.resolve() {
            PointerResult::Async(ptr) => unsafe {
                AsyncSignal::terminate(ptr);
            },
        }
    }
    #[inline(always)]
    pub(crate) unsafe fn cancel(&self) {
        match self.resolve() {
            PointerResult::Async(ptr) => unsafe {
                AsyncSignal::cancel(ptr);
            },
        }
    }
}

pub(crate) struct AsyncSignal<T> {
    state: Cell<usize>,
    data: UnsafeCell<MaybeUninit<T>>,
    waker: UnsafeCell<Waker>,
    _pinned: core::marker::PhantomPinned,
}

impl<T> Debug for AsyncSignal<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("AsyncSignal").field("state", &self.state.get()).finish()
    }
}

const fn no_op_waker() -> Waker {
    use core::task::{RawWaker, RawWakerVTable, Waker};

    const unsafe fn clone(_: *const ()) -> RawWaker {
        raw_waker()
    }
    const unsafe fn wake(_: *const ()) {}
    const unsafe fn wake_by_ref(_: *const ()) {}
    const unsafe fn drop(_: *const ()) {}

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    const unsafe fn raw_waker() -> RawWaker {
        RawWaker::new(core::ptr::null(), &VTABLE)
    }

    unsafe { Waker::from_raw(raw_waker()) }
}

use crate::{backoff, future::FutureState, local_atomic::usize_cell_compare_exchange_local};

impl<T> AsyncSignal<T> {
    #[inline(always)]
    pub(crate) const fn new_recv() -> Self {
        Self {
            state: Cell::new(Self::state_to_usize(FutureState::Unregistered)),
            data: UnsafeCell::new(MaybeUninit::uninit()),
            waker: UnsafeCell::new(no_op_waker()),
            _pinned: core::marker::PhantomPinned,
        }
    }

    #[inline(always)]
    pub(crate) const fn new_send(data: T) -> Self {
        Self {
            state: Cell::new(Self::state_to_usize(FutureState::Unregistered)),
            data: UnsafeCell::new(MaybeUninit::new(data)),
            waker: UnsafeCell::new(no_op_waker()),
            _pinned: core::marker::PhantomPinned,
        }
    }

    /// SAFETY: caller must guarantee this signal is already finished and is not
    /// shared in any wait queue
    pub(crate) unsafe fn reset_send(&mut self, data: T) {
        self.data.get_mut().write(data);
        self.state.set(Self::state_to_usize(FutureState::Pending));
    }

    #[inline(always)]
    pub(crate) const fn new_send_finished() -> Self {
        Self {
            state: Cell::new(Self::state_to_usize(FutureState::Success)),
            data: UnsafeCell::new(MaybeUninit::uninit()),
            waker: UnsafeCell::new(no_op_waker()),
            _pinned: core::marker::PhantomPinned,
        }
    }

    #[inline(always)]
    const fn state_to_usize(state: FutureState) -> usize {
        match state {
            FutureState::Success => UNLOCKED,
            FutureState::Unregistered => UNINIT,
            FutureState::Pending => LOCKED,
            FutureState::Failure => TERMINATED,
            _ => DONE,
        }
    }

    #[inline(always)]
    const fn usize_to_state(val: usize) -> FutureState {
        match val {
            UNLOCKED => FutureState::Success,
            UNINIT => FutureState::Unregistered,
            LOCKED => FutureState::Pending,
            TERMINATED => FutureState::Failure,
            _ => FutureState::Done,
        }
    }

    #[inline(always)]
    pub(crate) fn state(&self) -> FutureState {
        Self::usize_to_state(self.state.get())
    }

    #[inline(always)]
    pub(crate) fn set_state(&self, state: FutureState) {
        self.state.set(Self::state_to_usize(state));
    }

    pub(crate) fn set_state_relaxed(&self, state: FutureState) {
        self.state.set(Self::state_to_usize(state));
    }

    #[inline(always)]
    #[allow(unused)]
    pub(crate) fn compare_exchange_future_state(
        &self,
        current: FutureState,
        new: FutureState,
    ) -> Result<FutureState, FutureState> {
        match usize_cell_compare_exchange_local(
            &self.state,
            Self::state_to_usize(current),
            Self::state_to_usize(new),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(new),
            Err(v) => Err(Self::usize_to_state(v)),
        }
    }
    #[inline(always)]
    pub(crate) fn dynamic_ptr(&self) -> DynamicSignal<T> {
        DynamicSignal::new_async(self as *const AsyncSignal<T>)
    }
    #[inline(always)]

    pub(crate) fn as_tagged_ptr(&self) -> *const () {
        self as *const Self as *const ()
    }

    #[inline(always)]
    pub(crate) unsafe fn will_wake(&self, waker: &Waker) -> bool {
        (&*self.waker.get()).will_wake(waker)
    }
    // SAFETY: this function is only safe when owner of signal have exclusive lock
    // over channel,  this avoids another reader to clone the waker while we are
    // updating it.  this function should not be called if signal is
    // uninitialized or already shared.
    #[inline(always)]
    pub(crate) unsafe fn update_waker(&self, waker: &Waker) {
        *self.waker.get() = waker.clone();
    }
    #[inline(always)]
    pub(crate) unsafe fn clone_waker(&self) -> Waker {
        (&*self.waker.get()).clone()
    }
    #[inline(always)]
    pub(crate) unsafe fn write_data(this: *const Self, data: T) {
        let waker = (*this).clone_waker();
        (*this).data.get().write(MaybeUninit::new(data));
        (*this).state.set(UNLOCKED);
        waker.wake();
    }
    #[inline(always)]
    pub(crate) unsafe fn read_data(this: *const Self) -> T {
        let waker = (*this).clone_waker();
        let data = (*this).data.get().read().assume_init();
        (*this).state.set(UNLOCKED);
        waker.wake();
        data
    }

    // Drops waker without waking
    pub(crate) unsafe fn cancel(_this: *const Self) {}
    pub(crate) unsafe fn assume_init(&self) -> T {
        unsafe { self.data.get().read().assume_init() }
    }
    pub(crate) unsafe fn terminate(this: *const Self) {
        let waker = (*this).clone_waker();
        (*this).state.set(TERMINATED);
        waker.wake();
    }
    #[inline(always)]
    pub(crate) unsafe fn drop_data(&mut self) {
        let ptr = self.data.get();
        (&mut *ptr).as_mut_ptr().drop_in_place();
    }
    #[inline(never)]
    #[cold]
    pub(crate) fn blocking_wait(&self) -> bool {
        let v = self.state.get();
        if likely(v > LOCKED_STARVATION) {
            return v == UNLOCKED;
        }

        for _ in 0..32 {
            backoff::yield_os();
            let v = self.state.get();
            if likely(v > LOCKED_STARVATION) {
                return v == UNLOCKED;
            }
        }

        // Usually this part will not happen but you can't be sure
        let mut sleep_time: u64 = 1 << 10;
        loop {
            backoff::sleep(Duration::from_nanos(sleep_time));
            let v = self.state.get();
            if likely(v > LOCKED_STARVATION) {
                return v == UNLOCKED;
            }
            // increase sleep_time gradually to 262 microseconds
            if sleep_time < (1 << 18) {
                sleep_time <<= 1;
            }
        }
    }
}
