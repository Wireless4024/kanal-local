#![doc = include_str!("../README.md")]
#![warn(missing_docs, missing_debug_implementations)]

pub(crate) mod backoff;
pub(crate) mod internal;

mod error;
mod future;
mod local_atomic;
mod signal;

pub use error::*;
pub use future::*;

use core::fmt;
use std::collections::VecDeque;

use branches::unlikely;
use internal::{acquire_internal, try_acquire_internal, Internal};

/// Sending side of the channel with async API.  It's possible to convert it to
/// sync [`Sender`] with `as_sync`, `to_sync` or `clone_sync` based on software
/// requirement.
#[repr(C)]
pub struct AsyncSender<T> {
    internal: Internal<T>,
}

impl<T> Drop for AsyncSender<T> {
    fn drop(&mut self) {
        self.internal.drop_send();
    }
}

impl<T> Clone for AsyncSender<T> {
    fn clone(&self) -> Self {
        Self {
            internal: self.internal.clone_send(),
        }
    }
}

impl<T> fmt::Debug for AsyncSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AsyncSender {{ .. }}")
    }
}

macro_rules! check_recv_closed_timeout {
    ($internal:ident,$data:ident) => {
        if unlikely($internal.recv_count == 0) {
            // Avoid wasting lock time on dropping failed send object
            drop($internal);
            return Err(SendTimeoutError::Closed($data));
        }
    };
}

macro_rules! shared_impl {
    () => {
        /// Returns whether the channel is bounded or not.
        ///
        /// # Examples
        ///
        /// ```
        /// let (s, r) = kanal::bounded_async::<u64>(0);
        /// assert_eq!(s.is_bounded(),true);
        /// assert_eq!(r.is_bounded(),true);
        /// ```
        /// ```
        /// let (s, r) = kanal::unbounded_async::<u64>();
        /// assert_eq!(s.is_bounded(),false);
        /// assert_eq!(r.is_bounded(),false);
        /// ```
        pub fn is_bounded(&self) -> bool {
            self.internal.capacity() != usize::MAX
        }
        /// Returns length of the queue.
        pub fn len(&self) -> usize {
            acquire_internal(&self.internal).queue.len()
        }
        /// Returns whether the channel queue is empty or not.
        ///
        /// # Examples
        ///
        /// ```
        /// let (s, r) = kanal::unbounded_async::<u64>();
        /// assert_eq!(s.is_empty(),true);
        /// assert_eq!(r.is_empty(),true);
        /// ```
        pub fn is_empty(&self) -> bool {
            acquire_internal(&self.internal).queue.is_empty()
        }
        /// Returns whether the channel queue is full or not
        /// full channels will block on send and recv calls
        /// it always returns true for zero sized channels.
        pub fn is_full(&self) -> bool {
            self.internal.capacity() == acquire_internal(&self.internal).queue.len()
        }
        /// Returns capacity of channel (not the queue)
        /// for unbounded channels, it will return usize::MAX.
        pub fn capacity(&self) -> usize {
            self.internal.capacity()
        }
        /// Returns count of alive receiver instances of the channel.
        pub fn receiver_count(&self) -> usize {
            acquire_internal(&self.internal).recv_count as usize
        }
        /// Returns count of alive sender instances of the channel.
        pub fn sender_count(&self) -> usize {
            acquire_internal(&self.internal).send_count as usize
        }
        /// Closes the channel completely on both sides and terminates waiting
        /// signals.
        pub fn close(&self) -> Result<(), CloseError> {
            let mut internal = acquire_internal(&self.internal);
            if unlikely(internal.recv_count == 0 && internal.send_count == 0) {
                return Err(CloseError());
            }
            internal.recv_count = 0;
            internal.send_count = 0;
            internal.terminate_signals();
            internal.queue.clear();
            Ok(())
        }
        /// Returns whether the channel is closed on both side of send and
        /// receive or not.
        pub fn is_closed(&self) -> bool {
            let internal = acquire_internal(&self.internal);
            internal.send_count == 0 && internal.recv_count == 0
        }
    };
}

macro_rules! shared_send_impl {
    () => {
        /// Tries sending to the channel without waiting on the waitlist, if
        /// send fails then the object will be dropped. It returns `Ok(true)` in
        /// case of a successful operation and `Ok(false)` for a failed one, or
        /// error in case that channel is closed. Important note: this function
        /// is not lock-free as it acquires a mutex guard of the channel
        /// internal for a short time.
        #[inline(always)]
        pub fn try_send(&self, data: T) -> Result<(), SendTimeoutError<T>> {
            let cap = self.internal.capacity();
            let mut internal = acquire_internal(&self.internal);
            check_recv_closed_timeout!(internal, data);
            if let Some(first) = internal.next_recv() {
                drop(internal);
                // SAFETY: it's safe to send to owned signal once
                unsafe { first.send(data) }
                return Ok(());
            }
            if cap > 0 && internal.queue.len() < cap {
                internal.queue.push_back(data);
                return Ok(());
            }
            Err(SendTimeoutError::Timeout(data))
        }

        /// Tries sending to the channel without waiting on the waitlist or for
        /// the internal mutex, if send fails then the object will be dropped.
        /// It returns `Ok(true)` in case of a successful operation and
        /// `Ok(false)` for a failed one, or error in case that channel is
        /// closed. Do not use this function unless you know exactly what you
        /// are doing.
        #[inline(always)]
        pub fn try_send_realtime(&self, data: T) -> Result<(), SendTimeoutError<T>> {
            let cap = self.internal.capacity();
            if let Some(mut internal) = try_acquire_internal(&self.internal) {
                check_recv_closed_timeout!(internal, data);
                if let Some(first) = internal.next_recv() {
                    drop(internal);
                    // SAFETY: it's safe to send to owned signal once
                    unsafe { first.send(data) }
                    return Ok(());
                }
                if cap > 0 && internal.queue.len() < cap {
                    internal.queue.push_back(data);
                    return Ok(());
                }
            }
            Err(SendTimeoutError::Timeout(data))
        }

        /// Returns whether the receive side of the channel is closed or not.
        pub fn is_disconnected(&self) -> bool {
            acquire_internal(&self.internal).recv_count == 0
        }
    };
}

macro_rules! shared_recv_impl {
    () => {
        /// Tries receiving from the channel without waiting on the waitlist.
        /// It returns `Ok(Some(T))` in case of successful operation and
        /// `Ok(None)` for a failed one, or error in case that channel is
        /// closed. Important note: this function is not lock-free as it
        /// acquires a mutex guard of the channel internal for a short time.
        #[inline(always)]
        pub fn try_recv(&self) -> Result<Option<T>, ReceiveError> {
            let cap = self.internal.capacity();
            let mut internal = acquire_internal(&self.internal);
            if unlikely(internal.recv_count == 0) {
                return Err(ReceiveError());
            }
            if cap > 0 {
                if let Some(v) = internal.queue.pop_front() {
                    if let Some(p) = internal.next_send() {
                        // if there is a sender take its data and push it into the
                        // queue Safety: it's safe to receive from owned
                        // signal once
                        unsafe { internal.queue.push_back(p.recv()) }
                    }
                    return Ok(Some(v));
                }
            }
            if let Some(p) = internal.next_send() {
                // SAFETY: it's safe to receive from owned signal once
                drop(internal);
                return unsafe { Ok(Some(p.recv())) };
            }
            if unlikely(internal.send_count == 0) {
                return Err(ReceiveError());
            }
            Ok(None)
            // if the queue is not empty send the data
        }
        /// Tries receiving from the channel without waiting on the waitlist or
        /// waiting for channel internal lock. It returns `Ok(Some(T))` in case
        /// of successful operation and `Ok(None)` for a failed one, or error in
        /// case that channel is closed. Do not use this function unless you
        /// know exactly what you are doing.
        #[inline(always)]
        pub fn try_recv_realtime(&self) -> Result<Option<T>, ReceiveError> {
            let cap = self.internal.capacity();
            if let Some(mut internal) = try_acquire_internal(&self.internal) {
                if unlikely(internal.recv_count == 0) {
                    return Err(ReceiveError());
                }
                if cap > 0 {
                    if let Some(v) = internal.queue.pop_front() {
                        if let Some(p) = internal.next_send() {
                            // if there is a sender take its data and push it into
                            // the queue Safety: it's safe to
                            // receive from owned signal once
                            unsafe { internal.queue.push_back(p.recv()) }
                        }
                        return Ok(Some(v));
                    }
                }
                if let Some(p) = internal.next_send() {
                    // SAFETY: it's safe to receive from owned signal once
                    drop(internal);
                    return unsafe { Ok(Some(p.recv())) };
                }
                if unlikely(internal.send_count == 0) {
                    return Err(ReceiveError());
                }
            }
            Ok(None)
        }

        /// Drains all available messages from the channel into the provided vector and
        /// returns the number of received messages.
        ///
        /// The function is designed to be non-blocking, meaning it only processes
        /// messages that are readily available and returns immediately with whatever
        /// messages are present. It provides a count of received messages, which could
        /// be zero if no messages are available at the time of the call.
        ///
        /// When using this function, it’s a good idea to check if the returned count is
        /// zero to avoid busy-waiting in a loop. If blocking behavior is desired when
        /// the count is zero, you can use the `recv()` function if count is zero. For
        /// efficiency, reusing the same vector across multiple calls can help minimize
        /// memory allocations. Between uses, you can clear the vector with
        /// `vec.clear()` to prepare it for the next set of messages.
        pub fn drain_into(&self, vec: &mut Vec<T>) -> Result<usize, ReceiveError> {
            let vec_initial_length = vec.len();
            let remaining_cap = vec.capacity() - vec_initial_length;
            let mut internal = acquire_internal(&self.internal);
            if unlikely(internal.recv_count == 0) {
                return Err(ReceiveError());
            }
            let required_cap = internal.queue.len() + {
                if internal.recv_blocking {
                    0
                } else {
                    internal.wait_list.len()
                }
            };
            if required_cap > remaining_cap {
                vec.reserve(vec_initial_length + required_cap - remaining_cap);
            }
            while let Some(v) = internal.queue.pop_front() {
                vec.push(v);
            }
            while let Some(p) = internal.next_send() {
                // SAFETY: it's safe to receive from owned signal once
                unsafe { vec.push(p.recv()) }
            }
            Ok(required_cap)
        }

        /// Returns, whether the send side of the channel, is closed or not.
        pub fn is_disconnected(&self) -> bool {
            acquire_internal(&self.internal).send_count == 0
        }

        /// Returns, whether the channel receive side is terminated, and will
        /// not return any result in future recv calls.
        pub fn is_terminated(&self) -> bool {
            let internal = acquire_internal(&self.internal);
            internal.send_count == 0 && internal.queue.len() == 0
        }
    };
}

impl<T> AsyncSender<T> {
    /// Sends data asynchronously to the channel.
    #[inline(always)]
    pub fn send(&'_ self, data: T) -> SendFuture<'_, T> {
        SendFuture::new(&self.internal, data)
    }

    /// Sends multiple elements from a `VecDeque` into the channel
    /// asynchronously.
    ///
    /// This method consumes the provided `VecDeque` by repeatedly popping
    /// elements from its front and sending each one over the channel. The
    /// operation completes when the deque is empty or when the channel is
    /// closed.
    /// ```
    #[inline(always)]
    pub fn send_many<'a, 'b>(&'a self, elements: &'b mut VecDeque<T>) -> SendManyFuture<'a, 'b, T> {
        SendManyFuture::new(&self.internal, elements)
    }

    shared_send_impl!();
    shared_impl!();
}

/// [`AsyncReceiver`] is receiving side of the channel in async mode.
/// Receivers can be cloned and produce receivers to operate in both sync and
/// async modes.
#[repr(C)]
pub struct AsyncReceiver<T> {
    internal: Internal<T>,
}

impl<T> fmt::Debug for AsyncReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AsyncReceiver {{ .. }}")
    }
}

impl<T> AsyncReceiver<T> {
    /// Returns a [`ReceiveFuture`] to receive data from the channel
    /// asynchronously.
    ///
    /// # Cancellation and Polling Considerations
    ///
    /// Due to current limitations in Rust's handling of future cancellation, if
    /// a `ReceiveFuture` is dropped exactly at the time when new data is
    /// written to the channel, it may result in the loss of the received
    /// value. This behavior although memory-safe stems from the fact that
    /// Rust does not provide a built-in, correct mechanism for cancelling
    /// futures.
    ///
    /// Additionally, it is important to note that constructs such as
    /// `tokio::select!` are not correct to use with kanal async channels.
    /// Kanal's design does not rely on the conventional `poll` mechanism to
    /// read messages. Because of its internal optimizations, the future may
    /// complete without receiving the final poll, which prevents proper
    /// handling of the message.
    ///
    /// As a result, once the `ReceiveFuture` is polled for the first time
    /// (which registers the request to receive data), the programmer must
    /// commit to completing the polling process. This ensures that messages
    /// are correctly delivered and avoids potential race conditions associated
    /// with cancellation.
    #[inline(always)]
    pub fn recv(&'_ self) -> ReceiveFuture<'_, T> {
        ReceiveFuture::new_ref(&self.internal)
    }
    /// Creates a asynchronous stream for the channel to receive messages,
    /// [`ReceiveStream`] borrows the [`AsyncReceiver`], after dropping it,
    /// receiver will be available and usable again.
    #[inline(always)]
    pub fn stream(&'_ self) -> ReceiveStream<'_, T> {
        ReceiveStream::new_borrowed(self)
    }
    shared_recv_impl!();

    shared_impl!();
}
impl<T> Drop for AsyncReceiver<T> {
    fn drop(&mut self) {
        self.internal.drop_recv();
    }
}

impl<T> Clone for AsyncReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            internal: self.internal.clone_recv(),
        }
    }
}

/// Creates a new async bounded channel with the requested buffer size, and
/// returns [`AsyncSender`] and [`AsyncReceiver`] of the channel for type T, you
/// can get access to sync API of [`Sender`] and [`Receiver`] with `to_sync`,
/// `as_async` or `clone_sync` based on your requirements, by calling them on
/// async sender or receiver.
pub fn bounded_async<T>(size: usize) -> (AsyncSender<T>, AsyncReceiver<T>) {
    let internal = Internal::new(true, size);
    (
        AsyncSender {
            internal: internal.clone_unchecked(),
        },
        AsyncReceiver { internal },
    )
}

const UNBOUNDED_STARTING_SIZE: usize = 32;

/// Creates a new async unbounded channel, and returns [`AsyncSender`] and
/// [`AsyncReceiver`] of the channel for type T, you can get access to sync API
/// of [`Sender`] and [`Receiver`] with `to_sync`, `as_async` or `clone_sync`
/// based on your requirements, by calling them on async sender or receiver.
///
/// # Warning
/// This unbounded channel does not shrink its queue. As a result, if the
/// receive side is exhausted or delayed, the internal queue may grow
/// substantially. This behavior is intentional and considered as a warmup
/// phase. If such growth is undesirable, consider using a bounded channel with
/// an appropriate queue size.
pub fn unbounded_async<T>() -> (AsyncSender<T>, AsyncReceiver<T>) {
    let internal = Internal::new(false, UNBOUNDED_STARTING_SIZE);
    (
        AsyncSender {
            internal: internal.clone_unchecked(),
        },
        AsyncReceiver { internal },
    )
}
