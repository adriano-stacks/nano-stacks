//! Bounded hand-offs for externally supplied work.

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

/// The independent count and byte limits on one queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub items: usize,
    pub bytes: usize,
}

impl Limits {
    #[must_use]
    pub const fn new(items: usize, bytes: usize) -> Self {
        assert!(items > 0, "a queue needs at least one item slot");
        assert!(bytes > 0, "a queue needs at least one byte");
        Self { items, bytes }
    }
}

/// A queue's current load and cumulative shedding record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Status {
    pub items: usize,
    pub bytes: usize,
    pub item_limit: usize,
    pub byte_limit: usize,
    pub oldest_age: Option<Duration>,
    pub dropped: u64,
    pub saturations: u64,
}

/// Why an item could not reserve queue space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReserveError {
    Full,
    Closed,
}

/// A local drop-oldest buffer with independent count and byte bounds.
///
/// This is for callers that already own their synchronization and must keep the
/// newest network announcements. [`channel`] is the producer/consumer hand-off.
#[derive(Debug)]
pub struct Buffer<T> {
    limits: Limits,
    entries: VecDeque<Buffered<T>>,
    bytes: usize,
    dropped: u64,
    saturations: u64,
}

impl<T> Buffer<T> {
    #[must_use]
    pub const fn new(limits: Limits) -> Self {
        Self {
            limits,
            entries: VecDeque::new(),
            bytes: 0,
            dropped: 0,
            saturations: 0,
        }
    }

    /// Keep `value`, shedding the oldest entries until both limits admit it.
    ///
    /// A value larger than the whole byte budget is shed itself. The return value
    /// says whether the new value was retained.
    pub fn push(&mut self, value: T, bytes: usize) -> bool {
        if bytes > self.limits.bytes {
            self.dropped = self.dropped.saturating_add(1);
            self.saturations = self.saturations.saturating_add(1);
            return false;
        }

        let saturated = self.entries.len() >= self.limits.items
            || bytes > self.limits.bytes.saturating_sub(self.bytes);
        if saturated {
            self.saturations = self.saturations.saturating_add(1);
        }
        while self.entries.len() >= self.limits.items
            || bytes > self.limits.bytes.saturating_sub(self.bytes)
        {
            let oldest = self
                .entries
                .pop_front()
                .expect("a positive limit can only reject a non-empty buffer");
            self.bytes -= oldest.bytes;
            self.dropped = self.dropped.saturating_add(1);
        }

        self.bytes += bytes;
        self.entries.push_back(Buffered {
            value,
            bytes,
            at: Instant::now(),
        });
        true
    }

    /// Drain retained values in arrival order and release their byte budget.
    #[must_use]
    pub fn take(&mut self) -> Vec<T> {
        self.bytes = 0;
        self.entries.drain(..).map(|entry| entry.value).collect()
    }

    #[must_use]
    pub fn status(&self) -> Status {
        Status {
            items: self.entries.len(),
            bytes: self.bytes,
            item_limit: self.limits.items,
            byte_limit: self.limits.bytes,
            oldest_age: self.entries.front().map(|entry| entry.at.elapsed()),
            dropped: self.dropped,
            saturations: self.saturations,
        }
    }
}

#[derive(Debug)]
struct Buffered<T> {
    value: T,
    bytes: usize,
    at: Instant,
}

impl fmt::Display for ReserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("the queue is full"),
            Self::Closed => formatter.write_str("the queue is closed"),
        }
    }
}

impl std::error::Error for ReserveError {}

/// The value returned when a direct send cannot be admitted.
#[derive(Debug)]
pub struct SendError<T> {
    pub value: T,
    pub reason: ReserveError,
}

/// Create one count- and byte-bounded queue.
#[must_use]
pub fn channel<T>(limits: Limits) -> (Sender<T>, Receiver<T>) {
    let (sender, receiver) = mpsc::channel(limits.items);
    let accounting = Arc::new(Mutex::new(Accounting::new(limits)));
    (
        Sender {
            channel: sender,
            accounting: Arc::clone(&accounting),
        },
        Receiver {
            channel: receiver,
            accounting,
        },
    )
}

/// The cloneable producer side of a bounded queue.
pub struct Sender<T> {
    channel: mpsc::Sender<Entry<T>>,
    accounting: Arc<Mutex<Accounting>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            accounting: Arc::clone(&self.accounting),
        }
    }
}

impl<T> fmt::Debug for Sender<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sender")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl<T> Sender<T> {
    /// Reserve an item slot and its bytes before doing work that cannot be undone.
    pub fn try_reserve(&self, bytes: usize) -> Result<Permit<T>, ReserveError> {
        let mut accounting = lock(&self.accounting);
        if self.channel.is_closed() {
            accounting.dropped = accounting.dropped.saturating_add(1);
            drop(accounting);
            return Err(ReserveError::Closed);
        }
        if accounting.entries.len() >= accounting.limits.items
            || bytes > accounting.limits.bytes.saturating_sub(accounting.bytes)
        {
            accounting.dropped = accounting.dropped.saturating_add(1);
            accounting.saturations = accounting.saturations.saturating_add(1);
            drop(accounting);
            return Err(ReserveError::Full);
        }

        let id = accounting.reserve(bytes);
        match self.channel.clone().try_reserve_owned() {
            Ok(channel) => {
                drop(accounting);
                Ok(Permit {
                    channel: Some(channel),
                    accounting: Arc::clone(&self.accounting),
                    id,
                })
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                accounting.release(id);
                accounting.dropped = accounting.dropped.saturating_add(1);
                accounting.saturations = accounting.saturations.saturating_add(1);
                drop(accounting);
                Err(ReserveError::Full)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                accounting.release(id);
                accounting.dropped = accounting.dropped.saturating_add(1);
                drop(accounting);
                Err(ReserveError::Closed)
            }
        }
    }

    /// Send one value without waiting for capacity.
    pub fn try_send(&self, value: T, bytes: usize) -> Result<(), SendError<T>> {
        match self.try_reserve(bytes) {
            Ok(permit) => {
                permit.send(value);
                Ok(())
            }
            Err(reason) => Err(SendError { value, reason }),
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        lock(&self.accounting).status()
    }
}

/// Capacity held before a caller commits the work that will be queued.
#[must_use = "dropping a permit releases its byte and item reservation"]
pub struct Permit<T> {
    channel: Option<mpsc::OwnedPermit<Entry<T>>>,
    accounting: Arc<Mutex<Accounting>>,
    id: u64,
}

impl<T> Permit<T> {
    /// Fill this reservation. This cannot fail after [`Sender::try_reserve`].
    pub fn send(mut self, value: T) {
        let channel = self.channel.take().expect("an unused queue permit");
        let channel = channel.send(Entry { id: self.id, value });
        if channel.is_closed() {
            lock(&self.accounting).release(self.id);
        }
    }
}

impl<T> Drop for Permit<T> {
    fn drop(&mut self) {
        if self.channel.is_some() {
            lock(&self.accounting).release(self.id);
        }
    }
}

/// The single-consumer side of a bounded queue.
pub struct Receiver<T> {
    channel: mpsc::Receiver<Entry<T>>,
    accounting: Arc<Mutex<Accounting>>,
}

impl<T> fmt::Debug for Receiver<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Receiver")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.recv_lease().await.map(Lease::into_inner)
    }

    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        self.try_recv_lease().map(Lease::into_inner)
    }

    /// Receive work while retaining its queue budget until the lease is dropped.
    pub async fn recv_lease(&mut self) -> Option<Lease<T>> {
        let entry = self.channel.recv().await?;
        Some(Lease {
            value: Some(entry.value),
            accounting: Arc::clone(&self.accounting),
            id: entry.id,
        })
    }

    pub fn try_recv_lease(&mut self) -> Result<Lease<T>, mpsc::error::TryRecvError> {
        let entry = self.channel.try_recv()?;
        Ok(Lease {
            value: Some(entry.value),
            accounting: Arc::clone(&self.accounting),
            id: entry.id,
        })
    }

    #[must_use]
    pub fn status(&self) -> Status {
        lock(&self.accounting).status()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.status().items
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        while let Ok(entry) = self.channel.try_recv() {
            lock(&self.accounting).release(entry.id);
        }
    }
}

/// Received work whose memory remains charged until processing finishes.
#[must_use = "dropping the lease releases its byte and item reservation"]
pub struct Lease<T> {
    value: Option<T>,
    accounting: Arc<Mutex<Accounting>>,
    id: u64,
}

impl<T> Lease<T> {
    #[must_use]
    pub const fn value(&self) -> &T {
        self.value.as_ref().expect("a live queue lease")
    }

    #[must_use]
    pub fn into_inner(mut self) -> T {
        lock(&self.accounting).release(self.id);
        self.value.take().expect("a live queue lease")
    }
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        if self.value.is_some() {
            lock(&self.accounting).release(self.id);
        }
    }
}

struct Entry<T> {
    id: u64,
    value: T,
}

#[derive(Clone, Copy)]
struct Reservation {
    at: Instant,
    bytes: usize,
}

struct Accounting {
    limits: Limits,
    entries: BTreeMap<u64, Reservation>,
    bytes: usize,
    next_id: u64,
    dropped: u64,
    saturations: u64,
}

impl Accounting {
    const fn new(limits: Limits) -> Self {
        Self {
            limits,
            entries: BTreeMap::new(),
            bytes: 0,
            next_id: 0,
            dropped: 0,
            saturations: 0,
        }
    }

    fn reserve(&mut self, bytes: usize) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.bytes += bytes;
        self.entries.insert(
            id,
            Reservation {
                at: Instant::now(),
                bytes,
            },
        );
        id
    }

    fn release(&mut self, id: u64) {
        if let Some(reservation) = self.entries.remove(&id) {
            self.bytes -= reservation.bytes;
        }
    }

    fn status(&self) -> Status {
        Status {
            items: self.entries.len(),
            bytes: self.bytes,
            item_limit: self.limits.items,
            byte_limit: self.limits.bytes,
            oldest_age: self
                .entries
                .values()
                .map(|entry| entry.at)
                .min()
                .map(|at| at.elapsed()),
            dropped: self.dropped,
            saturations: self.saturations,
        }
    }
}

fn lock(accounting: &Mutex<Accounting>) -> MutexGuard<'_, Accounting> {
    accounting
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Buffer, Limits, ReserveError, channel};

    #[test]
    fn a_local_buffer_sheds_oldest_by_count_and_bytes() {
        let mut buffer = Buffer::new(Limits::new(2, 10));
        assert!(buffer.push("first", 6));
        assert!(buffer.push("second", 4));
        assert!(buffer.push("third", 5));

        let status = buffer.status();
        assert_eq!(status.items, 2);
        assert_eq!(status.bytes, 9);
        assert_eq!(status.dropped, 1);
        assert_eq!(status.saturations, 1);
        assert!(status.oldest_age.is_some());
        assert_eq!(buffer.take(), vec!["second", "third"]);
        assert_eq!(buffer.status().items, 0);
        assert_eq!(buffer.status().bytes, 0);
    }

    #[test]
    fn a_local_buffer_refuses_one_oversized_value_and_recovers() {
        let mut buffer = Buffer::new(Limits::new(2, 10));
        assert!(!buffer.push("oversized", 11));
        assert_eq!(buffer.status().dropped, 1);
        assert_eq!(buffer.status().saturations, 1);
        assert!(buffer.push("fits", 10));
        assert_eq!(buffer.take(), vec!["fits"]);
    }

    #[tokio::test]
    async fn bytes_are_reserved_before_work_and_released_on_receive() {
        let (sender, mut receiver) = channel(Limits::new(2, 10));
        let permit = sender.try_reserve(7).expect("reserve before mutation");
        assert_eq!(sender.status().items, 1);
        assert_eq!(sender.status().bytes, 7);

        permit.send("accepted");
        assert_eq!(receiver.recv().await, Some("accepted"));
        assert_eq!(sender.status().items, 0);
        assert_eq!(sender.status().bytes, 0);
    }

    #[test]
    fn count_and_byte_saturation_drop_without_overcommitting() {
        let (sender, _receiver) = channel::<u8>(Limits::new(2, 10));
        let first = sender.try_reserve(6).expect("first reservation");
        assert_eq!(sender.try_reserve(5).err(), Some(ReserveError::Full));
        let second = sender.try_reserve(4).expect("remaining bytes");
        assert_eq!(sender.try_reserve(0).err(), Some(ReserveError::Full));

        let status = sender.status();
        assert_eq!(status.items, 2);
        assert_eq!(status.bytes, 10);
        assert_eq!(status.dropped, 2);
        assert_eq!(status.saturations, 2);
        drop((first, second));
        assert!(sender.status().oldest_age.is_none());
    }

    #[test]
    fn an_oversized_item_never_reserves_memory() {
        let (sender, _receiver) = channel::<u8>(Limits::new(2, 10));
        assert_eq!(sender.try_reserve(11).err(), Some(ReserveError::Full));
        assert_eq!(sender.status().bytes, 0);
    }

    #[tokio::test]
    async fn age_and_capacity_recover_after_a_saturated_queue_drains() {
        let (sender, mut receiver) = channel(Limits::new(1, 8));
        sender.try_send(1, 8).expect("fill the queue");
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(sender.status().oldest_age.is_some_and(|age| !age.is_zero()));
        assert_eq!(
            sender.try_send(2, 1).expect_err("full").reason,
            ReserveError::Full
        );
        assert_eq!(receiver.recv().await, Some(1));
        sender.try_send(3, 8).expect("capacity recovered");
        assert_eq!(receiver.recv().await, Some(3));
    }

    #[tokio::test]
    async fn a_receive_lease_holds_capacity_until_work_finishes() {
        let (sender, mut receiver) = channel(Limits::new(1, 8));
        sender.try_send(1, 8).expect("fill the queue");
        let lease = receiver.recv_lease().await.expect("receive work");
        assert_eq!(*lease.value(), 1);
        assert_eq!(sender.status().bytes, 8);
        assert_eq!(sender.try_reserve(1).err(), Some(ReserveError::Full));

        drop(lease);
        sender.try_send(2, 8).expect("capacity recovered");
        assert_eq!(receiver.recv().await, Some(2));
    }

    #[test]
    fn dropping_the_receiver_closes_and_clears_the_queue() {
        let (sender, receiver) = channel(Limits::new(1, 8));
        sender.try_send(1, 8).expect("queue one item");
        drop(receiver);
        assert_eq!(sender.status().bytes, 0);
        assert_eq!(sender.try_reserve(1).err(), Some(ReserveError::Closed));
        assert_eq!(sender.status().dropped, 1);
    }
}
