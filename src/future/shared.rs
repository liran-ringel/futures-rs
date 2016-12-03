//! Definition of the Shared combinator, a future that is cloneable,
//! and can be polled in multiple threads.

use std::mem;
use std::vec::Vec;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::ops::Deref;

use {Future, Poll, Async};
use task::{self, Task};
use lock::Lock;


/// Trait that adds the `shared` method to every `Future`.
///
/// # Examples
///
/// ```
/// use futures::future::*;
///
/// let future = ok::<_, bool>(6);
/// let shared1 = future.shared();
/// let shared2 = shared1.clone();
/// assert_eq!(6, *shared1.wait().unwrap());
/// assert_eq!(6, *shared2.wait().unwrap());
/// ```
pub trait IntoShared {
    /// Convert this future into `Shared` future.
    ///
    /// The shared() method provides a mean to convert any future into a cloneable future.
    /// It enables a future to be polled by multiple threads.
    ///
    /// `Shared` contains finishes with `SharedItem<T>` where T is the original future item,
    /// or with `SharedError<E>` where E is the original future item.
    /// Both `SharedItem` and `SharedError` implements `Deref`,
    /// so only a deref is required in order to access the item/error.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures::future::*;
    ///
    /// let future = ok::<_, bool>(6);
    /// let shared1 = future.shared();
    /// let shared2 = shared1.clone();
    /// assert_eq!(6, *shared1.wait().unwrap());
    /// assert_eq!(6, *shared2.wait().unwrap());
    /// ```
    ///
    /// ```
    /// use std::thread;
    /// use futures::future::*;
    ///
    /// let future = ok::<_, bool>(6);
    /// let shared1 = future.shared();
    /// let shared2 = shared1.clone();
    /// let join_handle = thread::spawn(move || {
    ///     assert_eq!(6, *shared2.wait().unwrap());
    /// });
    /// assert_eq!(6, *shared1.wait().unwrap());
    /// join_handle.join().unwrap();
    /// ```
    fn shared(self) -> Shared<Self> where Self: Future + Sized;
}

impl<T> IntoShared for T
    where T: Future + Sized
{
    fn shared(self) -> Shared<Self> {
        Shared::new(self)
    }
}

/// A future that is cloneable and can be polled in multiple threads.
/// Use Future::shared() method to convert any future into a `Shared` future.
#[must_use = "futures do nothing unless polled"]
pub struct Shared<F>
    where F: Future
{
    inner: Arc<Inner<F>>,
}

struct Inner<F>
    where F: Future
{
    /// The original future.
    original_future: Lock<F>,
    /// Indicates whether the result is ready, and the state is `State::Done`.
    result_ready: AtomicBool,
    /// The state of the shared future.
    state: RwLock<State<F::Item, F::Error>>,
}

/// The state of the shared future. It can be one of the following:
/// 1. Done - contains the result of the original future.
/// 2. Waiting - contains the waiting tasks.
enum State<T, E> {
    Waiting(Vec<Task>),
    Done(Result<SharedItem<T>, SharedError<E>>),
}

impl<F> Shared<F>
    where F: Future
{
    /// Creates a new `Shared` from another future.
    pub fn new(future: F) -> Self {
        Shared {
            inner: Arc::new(Inner {
                original_future: Lock::new(future),
                result_ready: AtomicBool::new(false),
                state: RwLock::new(State::Waiting(vec![])),
            }),
        }
    }

    /// Converts a result as it's stored in `State::Done` into `Poll`.
    fn result_to_polled_result(result: Result<SharedItem<F::Item>, SharedError<F::Error>>)
                               -> Result<Async<SharedItem<F::Item>>, SharedError<F::Error>> {
        match result {
            Ok(item) => Ok(Async::Ready(item)),
            Err(error) => Err(error),
        }
    }

    /// Clones the result from self.inner.state.
    /// Assumes state is `State::Done`.
    fn read_result(&self) -> Result<Async<SharedItem<F::Item>>, SharedError<F::Error>> {
        match *self.inner.state.read().unwrap() {
            State::Done(ref result) => Self::result_to_polled_result(result.clone()),
            State::Waiting(_) => panic!("read_result() was called but State is not Done"),
        }
    }

    /// Stores the result in self.inner.state, unparks the waiting tasks,
    /// and returns the result.
    fn store_result(&self,
                    result: Result<SharedItem<F::Item>, SharedError<F::Error>>)
                    -> Result<Async<SharedItem<F::Item>>, SharedError<F::Error>> {
        let ref mut state = *self.inner.state.write().unwrap();

        match mem::replace(state, State::Done(result.clone())) {
            State::Waiting(waiters) => {
                self.inner.result_ready.store(true, Ordering::Relaxed);
                for task in waiters {
                    task.unpark();
                }
            }
            State::Done(_) => panic!("store_result() was called twice"),
        }

        Self::result_to_polled_result(result)
    }
}

impl<F> Future for Shared<F>
    where F: Future
{
    type Item = SharedItem<F::Item>;
    type Error = SharedError<F::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // The logic is as follows:
        // 1. Check if the result is ready (with result_ready)
        //  - If the result is ready, return it.
        //  - Otherwise:
        // 2. Try lock the self.inner.original_future:
        //    - If successfully locked, check again if the result is ready.
        //      If it's ready, just return it.
        //      Otherwise, poll the original future.
        //      If the future is ready, unpark the waiting tasks from
        //      self.inner.state and return the result.
        //    - If the future is not ready, or if the lock failed:
        // 3. Lock the state for write.
        // 4. If the state is `State::Done`, return the result. Otherwise:
        // 5. Create a task, push it to the waiters vector, and return `Ok(Async::NotReady)`.

        // If the result is ready, just return it
        if self.inner.result_ready.load(Ordering::Relaxed) {
            return self.read_result();
        }

        // The result was not ready.
        // Try lock the original future.
        match self.inner.original_future.try_lock() {
            Some(mut original_future) => {
                // Other thread could already poll the result, so we check if result_ready.
                if self.inner.result_ready.load(Ordering::Relaxed) {
                    return self.read_result();
                }

                match original_future.poll() {
                    Ok(Async::Ready(item)) => {
                        return self.store_result(Ok(SharedItem::new(item)));
                    }
                    Err(error) => {
                        return self.store_result(Err(SharedError::new(error)));
                    }
                    Ok(Async::NotReady) => {} // A task will be parked
                }
            }
            None => {} // A task will be parked
        }

        let ref mut state = *self.inner.state.write().unwrap();
        match state {
            &mut State::Done(ref result) => return Self::result_to_polled_result(result.clone()),
            &mut State::Waiting(ref mut waiters) => {
                waiters.push(task::park());
            }
        }

        Ok(Async::NotReady)
    }
}

impl<F> Clone for Shared<F>
    where F: Future
{
    fn clone(&self) -> Self {
        Shared { inner: self.inner.clone() }
    }
}

/// A wrapped item of the original future.
/// It is clonable and implements Deref for ease of use.
#[derive(Debug)]
pub struct SharedItem<T> {
    item: Arc<T>,
}

impl<T> SharedItem<T> {
    fn new(item: T) -> Self {
        SharedItem { item: Arc::new(item) }
    }
}

impl<T> Clone for SharedItem<T> {
    fn clone(&self) -> Self {
        SharedItem { item: self.item.clone() }
    }
}

impl<T> Deref for SharedItem<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.item.as_ref()
    }
}

/// A wrapped error of the original future.
/// It is clonable and implements Deref for ease of use.
#[derive(Debug)]
pub struct SharedError<E> {
    error: Arc<E>,
}

impl<E> SharedError<E> {
    fn new(error: E) -> Self {
        SharedError { error: Arc::new(error) }
    }
}

impl<T> Clone for SharedError<T> {
    fn clone(&self) -> Self {
        SharedError { error: self.error.clone() }
    }
}

impl<E> Deref for SharedError<E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.error.as_ref()
    }
}