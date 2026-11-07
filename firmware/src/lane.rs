//! An unbounded queue between two tasks on one executor.
//!
//! A task that sends to a lane never waits, so two tasks that send to each
//! other cannot block each other. The pattern comes from the `cpb` example of
//! Crux (`examples/counter-http/cpb/src/delay.rs`).

use alloc::{collections::VecDeque, vec::Vec};
use core::cell::RefCell;

use embassy_sync::{
    blocking_mutex::{Mutex, raw::CriticalSectionRawMutex},
    signal::Signal,
};

/// An unbounded queue with a signal for the task that reads it.
pub struct Lane<T> {
    queue: Mutex<CriticalSectionRawMutex, RefCell<VecDeque<T>>>,
    ready: Signal<CriticalSectionRawMutex, ()>,
}

impl<T> Default for Lane<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Lane<T> {
    /// Makes an empty lane.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            queue: Mutex::new(RefCell::new(VecDeque::new())),
            ready: Signal::new(),
        }
    }

    /// Adds an item to the end of the queue and wakes the reader.
    pub fn push(&self, item: T) {
        self.queue.lock(|queue| queue.borrow_mut().push_back(item));
        self.ready.signal(());
    }

    /// Waits for an item and removes it from the front of the queue.
    pub async fn pop(&self) -> T {
        loop {
            if let Some(item) = self.try_pop() {
                return item;
            }
            self.ready.wait().await;
        }
    }

    /// Removes the item at the front of the queue, if there is one.
    pub fn try_pop(&self) -> Option<T> {
        self.queue.lock(|queue| queue.borrow_mut().pop_front())
    }

    /// Removes all the items, from the front of the queue.
    pub fn take_all(&self) -> Vec<T> {
        self.queue
            .lock(|queue| queue.borrow_mut().drain(..).collect())
    }
}
