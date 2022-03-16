use async_stream::stream;
use crossbeam_queue::ArrayQueue;
use futures::Stream;
use std::{
    cmp, fmt,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::Bufferable;

/// Error returned by `LimitedSender::send` when the receiver has disconnected.
#[derive(Debug, PartialEq)]
pub struct SendError<T>(pub T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "receiver disconnected")
    }
}

impl<T: fmt::Debug> std::error::Error for SendError<T> {}

/// Error returned by `LimitedSender::try_send`.
#[derive(Debug, PartialEq)]
pub enum TrySendError<T> {
    InsufficientCapacity(T),
    Disconnected(T),
}

impl<T> TrySendError<T> {
    pub fn into_inner(self) -> T {
        match self {
            Self::InsufficientCapacity(item) | Self::Disconnected(item) => item,
        }
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientCapacity(_) => {
                write!(fmt, "channel lacks sufficient capacity for send")
            }
            Self::Disconnected(_) => write!(fmt, "receiver disconnected"),
        }
    }
}

impl<T: fmt::Debug> std::error::Error for TrySendError<T> {}

#[derive(Debug)]
struct Inner<T> {
    data: Arc<ArrayQueue<(OwnedSemaphorePermit, T)>>,
    limit: usize,
    limiter: Arc<Semaphore>,
    read_waker: Arc<Notify>,
}

impl<T> Clone for Inner<T> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
            limit: self.limit,
            limiter: self.limiter.clone(),
            read_waker: self.read_waker.clone(),
        }
    }
}

#[derive(Debug)]
pub struct LimitedSender<T> {
    inner: Inner<T>,
    sender_count: Arc<AtomicUsize>,
}

impl<T: Bufferable> LimitedSender<T> {
    #[allow(clippy::cast_possible_truncation)]
    fn get_required_permits_for_item(&self, item: &T) -> u32 {
        // We have to limit the number of permits we ask for to the overall limit since we're always
        // willing to store more items than the limit if the queue is entirely empty, because
        // otherwise we might deadlock ourselves by not being able to send a single item.
        cmp::min(self.inner.limit, item.event_count()) as u32
    }

    /// Gets the number of items that this channel could accept.
    pub fn available_capacity(&self) -> usize {
        self.inner.limiter.available_permits()
    }

    /// Sends an item into the channel.
    pub async fn send(&mut self, item: T) -> Result<(), SendError<T>> {
        // Calculate how many permits we need, and wait until we can acquire all of them.
        let permits_required = self.get_required_permits_for_item(&item);
        info!("Trying to send item... ({} permits needed, {} available, {} max)",
            permits_required, self.available_capacity(), self.inner.limit);
        let permits = match self
            .inner
            .limiter
            .clone()
            .acquire_many_owned(permits_required)
            .await
        {
            Ok(permits) => permits,
            Err(_) => return Err(SendError(item)),
        };

        info!("Got the required {} permits, pushing items in.", permits_required);

        self.inner
            .data
            .push((permits, item))
            .expect("acquired permits but channel reported being full");
        self.inner.read_waker.notify_one();

        Ok(())
    }

    /// Attempts to send an item into the channel.
    pub fn try_send(&mut self, item: T) -> Result<(), TrySendError<T>> {
        // Calculate how many permits we need, and try to acquire them all without waiting.
        let permits_required = self.get_required_permits_for_item(&item);
        let permits = match self
            .inner
            .limiter
            .clone()
            .try_acquire_many_owned(permits_required)
        {
            Ok(permits) => permits,
            Err(ae) => {
                return match ae {
                    TryAcquireError::NoPermits => Err(TrySendError::InsufficientCapacity(item)),
                    TryAcquireError::Closed => Err(TrySendError::Disconnected(item)),
                }
            }
        };

        self.inner
            .data
            .push((permits, item))
            .expect("acquired permits but channel reported being full");
        self.inner.read_waker.notify_one();

        Ok(())
    }
}

impl<T> Clone for LimitedSender<T> {
    fn clone(&self) -> Self {
        self.sender_count.fetch_add(1, Ordering::SeqCst);

        Self {
            inner: self.inner.clone(),
            sender_count: Arc::clone(&self.sender_count),
        }
    }
}

impl<T> Drop for LimitedSender<T> {
    fn drop(&mut self) {
        // If we're the last sender to drop, close the semaphore on our way out the door.
        if self.sender_count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.inner.limiter.close();
            self.inner.read_waker.notify_one();
        }
    }
}

#[derive(Debug)]
pub struct LimitedReceiver<T> {
    inner: Inner<T>,
}

impl<T: Send + 'static> LimitedReceiver<T> {
    /// Gets the number of items that this channel could accept.
    pub fn available_capacity(&self) -> usize {
        self.inner.limiter.available_permits()
    }

    pub async fn next(&mut self) -> Option<T> {
        loop {
            if let Some((_permit, item)) = self.inner.data.pop() {
                info!("Received item!");
                return Some(item);
            }

            // There wasn't an item for us to pop, so see if the channel is actually closed.  If so,
            // then it's time for us to close up shop as well.
            if self.inner.limiter.is_closed() {
                return None;
            }

            // We're not closed, so we need to wait for a writer to tell us they made some
            // progress.  This might end up being a spurious wakeup since `Notify` will
            // store up to one wakeup that gets consumed by the next call to `poll_notify`,
            // but alas.
            info!("Need to wait for writer to make progress...");
            self.inner.read_waker.notified().await;
            info!("Writer made progress. Looping.");
        }
    }

    pub fn into_stream(self) -> Pin<Box<dyn Stream<Item = T> + Send>> {
        let mut receiver = self;
        Box::pin(stream! {
            while let Some(item) = receiver.next().await {
                yield item;
            }
        })
    }
}

impl<T> Drop for LimitedReceiver<T> {
    fn drop(&mut self) {
        // Notify senders that the channel is now closed by closing the semaphore.  Any pending
        // acquisitions will be awoken and notified that the semaphore is closed, and further new
        // sends will immediately see the semaphore is closed.
        self.inner.limiter.close();
    }
}

pub fn limited<T>(limit: usize) -> (LimitedSender<T>, LimitedReceiver<T>) {
    info!("Created limited channel with capacity limit of {}.", limit);
    let inner = Inner {
        data: Arc::new(ArrayQueue::new(limit)),
        limit,
        limiter: Arc::new(Semaphore::new(limit)),
        read_waker: Arc::new(Notify::new()),
    };

    let sender = LimitedSender {
        inner: inner.clone(),
        sender_count: Arc::new(AtomicUsize::new(1)),
    };
    let receiver = LimitedReceiver { inner };

    (sender, receiver)
}

/*#[cfg(test)]
mod tests {
    use futures::future::poll_fn;
    use tokio_test::{assert_pending, assert_ready, task::spawn};

    use crate::{test::common::MultiEventRecord, topology::channel::limited_queue::SendError};

    use super::limited;

    #[test]
    fn send_receive() {
        let (mut tx, mut rx) = limited(2);

        assert_eq!(2, tx.available_capacity());

        // Create our send and receive futures.
        let mut send = spawn(async {
            let msg: u64 = 42;

            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(msg)?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        let mut recv = spawn(poll_fn(|cx| rx.poll_next(cx)));

        // Nobody should be woken up.
        assert!(!send.is_woken());
        assert!(!recv.is_woken());

        // Try polling our receive, which should be pending because we haven't anything yet.
        assert_pending!(recv.poll());

        // We should immediately be able to complete a send as there is available capacity.
        assert_eq!(Ok(()), assert_ready!(send.poll()));

        // Now our receive should have been woken up, and should immediately be ready.
        assert!(recv.is_woken());
        assert_eq!(Some(42), assert_ready!(recv.poll()));
    }

    #[test]
    fn sender_waits_for_more_capacity_when_none_available() {
        let (mut tx, mut rx) = limited(1);

        assert_eq!(1, tx.available_capacity());

        // Create our send and receive futures.
        let mut send1 = spawn(async {
            let msg: u64 = 42;

            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(msg)?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        let mut recv1 = spawn(poll_fn(|cx| rx.poll_next(cx)));

        // Nobody should be woken up.
        assert!(!send1.is_woken());
        assert!(!recv1.is_woken());

        // Try polling our receive, which should be pending because we haven't anything yet.
        assert_pending!(recv1.poll());

        // We should immediately be able to complete a send as there is available capacity.
        assert_eq!(Ok(()), assert_ready!(send1.poll()));
        drop(send1);

        assert_eq!(0, tx.available_capacity());

        // Now our receive should have been woken up, and should immediately be ready... but we
        // aren't going to read the value just yet.
        assert!(recv1.is_woken());

        // Now trigger a second send, which should block as there's no available capacity.
        let mut send2 = spawn(async {
            let msg: u64 = 43;

            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(msg)?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        assert!(!send2.is_woken());
        assert_pending!(send2.poll());

        // Now if we receive the item, our second send should be woken up and be able to send in.
        assert_eq!(Some(42), assert_ready!(recv1.poll()));
        drop(recv1);

        assert_eq!(1, rx.available_capacity());

        let mut recv2 = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert!(!recv2.is_woken());
        assert_pending!(recv2.poll());

        assert!(send2.is_woken());
        assert_eq!(Ok(()), assert_ready!(send2.poll()));
        drop(send2);

        assert_eq!(0, tx.available_capacity());

        // And the final receive to get our second send.
        assert!(recv2.is_woken());
        assert_eq!(Some(43), assert_ready!(recv2.poll()));

        assert_eq!(1, tx.available_capacity());
    }

    #[test]
    fn sender_waits_for_more_capacity_when_partial_available() {
        let (mut tx, mut rx) = limited(7);

        assert_eq!(7, tx.available_capacity());

        // Create our send and receive futures.
        let mut small_sends = spawn(async {
            let msgs = vec![
                MultiEventRecord(1),
                MultiEventRecord(2),
                MultiEventRecord(3),
            ];

            for msg in msgs {
                poll_fn(|cx| tx.poll_ready(cx)).await?;
                tx.start_send(msg)?;
                poll_fn(|cx| tx.poll_flush(cx)).await?;
            }

            Ok::<_, SendError<MultiEventRecord>>(())
        });

        let mut recv1 = spawn(poll_fn(|cx| rx.poll_next(cx)));

        // Nobody should be woken up.
        assert!(!small_sends.is_woken());
        assert!(!recv1.is_woken());

        // Try polling our receive, which should be pending because we haven't anything yet.
        assert_pending!(recv1.poll());

        // We should immediately be able to complete our three event sends, which we have
        // available capacity for, but will consume all but one of the available slots.
        assert_eq!(Ok(()), assert_ready!(small_sends.poll()));
        drop(small_sends);

        assert_eq!(1, tx.available_capacity());

        // Now our receive should have been woken up, and should immediately be ready, but we won't
        // receive just yet.
        assert!(recv1.is_woken());

        // Now trigger a second send that has four events, and needs to wait for two receives to happen.
        let mut send2 = spawn(async {
            let msg = MultiEventRecord(4);

            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(msg)?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        assert!(!send2.is_woken());
        assert_pending!(send2.poll());

        // Now if we receive the first item, our second send should be woken up but still not able
        // to send.
        assert_eq!(Some(MultiEventRecord(1)), assert_ready!(recv1.poll()));
        drop(recv1);

        // Callers waiting to acquire permits have the permits immediately transfer to them when one
        // (or more) are released, so we expect this to be zero until we send and then read the
        // third item.
        assert_eq!(0, rx.available_capacity());

        // We don't get woken up until all permits have been acquired.
        assert!(!send2.is_woken());

        // Our second read should unlock enough available capacity for the second send once complete.
        let mut recv2 = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert!(!recv2.is_woken());
        assert_eq!(Some(MultiEventRecord(2)), assert_ready!(recv2.poll()));
        drop(recv2);

        assert_eq!(0, rx.available_capacity());

        assert!(send2.is_woken());
        assert_eq!(Ok(()), assert_ready!(send2.poll()));

        // And just make sure we see those last two sends.
        let mut recv3 = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert!(!recv3.is_woken());
        assert_eq!(Some(MultiEventRecord(3)), assert_ready!(recv3.poll()));
        drop(recv3);

        assert_eq!(3, rx.available_capacity());

        let mut recv4 = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert!(!recv4.is_woken());
        assert_eq!(Some(MultiEventRecord(4)), assert_ready!(recv4.poll()));
        drop(recv4);

        assert_eq!(7, rx.available_capacity());
    }

    #[test]
    fn empty_receiver_returns_none_when_last_sender_drops() {
        let (mut tx, mut rx) = limited(1);

        assert_eq!(1, tx.available_capacity());

        let tx2 = tx.clone();

        // Create our send and receive futures.
        let mut send = spawn(async {
            let msg: u64 = 42;

            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(msg)?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        let mut recv = spawn(poll_fn(|cx| rx.poll_next(cx)));

        // Nobody should be woken up.
        assert!(!send.is_woken());
        assert!(!recv.is_woken());

        // Try polling our receive, which should be pending because we haven't anything yet.
        assert_pending!(recv.poll());

        // Now drop our second sender, which shouldn't do anything yet.
        drop(tx2);
        assert!(!recv.is_woken());
        assert_pending!(recv.poll());

        // Now drop our second sender, but not before doing a send, which should trigger closing the
        // semaphore which should let the receiver complete with no further waiting: one item and
        // then `None`.
        assert_eq!(Ok(()), assert_ready!(send.poll()));
        drop(send);
        drop(tx);

        assert!(recv.is_woken());
        assert_eq!(Some(42), assert_ready!(recv.poll()));
        drop(recv);

        let mut recv2 = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert!(!recv2.is_woken());
        assert_eq!(None, assert_ready!(recv2.poll()));
    }

    #[test]
    fn receiver_returns_none_once_empty_when_last_sender_drops() {
        let (tx, mut rx) = limited::<u64>(1);

        assert_eq!(1, tx.available_capacity());

        let tx2 = tx.clone();

        // Create our receive future.
        let mut recv = spawn(poll_fn(|cx| rx.poll_next(cx)));

        // Nobody should be woken up.
        assert!(!recv.is_woken());

        // Try polling our receive, which should be pending because we haven't anything yet.
        assert_pending!(recv.poll());

        // Now drop our first sender, which shouldn't do anything yet.
        drop(tx);
        assert!(!recv.is_woken());
        assert_pending!(recv.poll());

        // Now drop our second sender, which should trigger closing the semaphore which should let
        // the receive complete as there are no items to read.
        drop(tx2);
        assert!(recv.is_woken());
        assert_eq!(None, assert_ready!(recv.poll()));
    }

    #[test]
    fn oversized_send_allowed_when_empty() {
        let (mut tx, mut rx) = limited(1);

        assert_eq!(1, tx.available_capacity());

        // Create our send and receive futures.
        let mut send = spawn(async {
            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(MultiEventRecord(2))?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        let mut recv = spawn(poll_fn(|cx| rx.poll_next(cx)));

        // Nobody should be woken up.
        assert!(!send.is_woken());
        assert!(!recv.is_woken());

        // We should immediately be able to complete our send, which we don't have full
        // available capacity for, but will consume all of the available slots.
        assert_eq!(Ok(()), assert_ready!(send.poll()));
        drop(send);

        assert_eq!(0, tx.available_capacity());

        // Now we should be able to get back the oversized item, but our capacity should not be
        // greater than what we started with.
        assert_eq!(Some(MultiEventRecord(2)), assert_ready!(recv.poll()));
        drop(recv);

        assert_eq!(1, rx.available_capacity());
    }

    #[test]
    fn oversized_send_allowed_when_partial_capacity() {
        let (mut tx, mut rx) = limited(2);

        assert_eq!(2, tx.available_capacity());

        // Create our send future.
        let mut send = spawn(async {
            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(MultiEventRecord(1))?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        // Nobody should be woken up.
        assert!(!send.is_woken());

        // We should immediately be able to complete our send, which will only use up a single slot.
        assert_eq!(Ok(()), assert_ready!(send.poll()));
        drop(send);

        assert_eq!(1, tx.available_capacity());

        // Now we'll trigger another send which has an oversized item.  It shouldn't be able to send
        // until all permits are available.
        let mut send2 = spawn(async {
            poll_fn(|cx| tx.poll_ready(cx)).await?;
            tx.start_send(MultiEventRecord(3))?;
            poll_fn(|cx| tx.poll_flush(cx)).await
        });

        assert!(!send2.is_woken());
        assert_pending!(send2.poll());

        assert_eq!(0, rx.available_capacity());

        // Now do a receive which should return the one consumed slot, essentially allowing all
        // permits to be acquired by the blocked send.
        let mut recv = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert!(!recv.is_woken());
        assert!(!send2.is_woken());

        assert_eq!(Some(MultiEventRecord(1)), assert_ready!(recv.poll()));
        drop(recv);

        assert_eq!(0, rx.available_capacity());

        // Now our blocked send should be able to proceed, and we should be able to read back the
        // item.
        assert_eq!(Ok(()), assert_ready!(send2.poll()));
        drop(send2);

        assert_eq!(0, tx.available_capacity());

        let mut recv2 = spawn(poll_fn(|cx| rx.poll_next(cx)));
        assert_eq!(Some(MultiEventRecord(3)), assert_ready!(recv2.poll()));

        assert_eq!(2, tx.available_capacity());
    }
}*/
