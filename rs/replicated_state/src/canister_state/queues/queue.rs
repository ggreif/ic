use crate::StateError;
#[cfg(test)]
mod tests;

use ic_protobuf::proxy::ProxyDecodeError;
use ic_protobuf::state::{ingress::v1 as pb_ingress, queues::v1 as pb_queues};
use ic_types::messages::{Ingress, Request, RequestOrResponse, Response};
use ic_types::{CountBytes, Cycles, Time};
use std::{
    collections::VecDeque,
    convert::{From, TryFrom, TryInto},
    mem::size_of,
    sync::Arc,
};

/// A FIFO queue that enforces an upper bound on the number of slots used and
/// reserved. Pushing an item into the queue or reserving a slot may fail if the
/// queue is full. Pushing an item into a reserved slot will always succeed
/// (unless a reservation has not been made, in which case it will panic).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct QueueWithReservation<T: std::clone::Clone> {
    queue: VecDeque<T>,
    /// Maximum number of messages allowed in the `queue` above.
    capacity: usize,
    /// Number of slots in the above `queue` currently reserved.  A slot must
    /// first be reserved before it can be pushed to which consumes it.
    num_slots_reserved: usize,
}

impl<T: std::clone::Clone> QueueWithReservation<T> {
    fn new(capacity: usize) -> Self {
        let queue = VecDeque::new();

        Self {
            queue,
            capacity,
            num_slots_reserved: 0,
        }
    }

    /// Returns `Ok(())` if there exists at least one available slot,
    /// `Err(StateError::QueueFull)` otherwise.
    fn check_has_slot(&self) -> Result<(), StateError> {
        if self.queue.len() + self.num_slots_reserved >= self.capacity {
            return Err(StateError::QueueFull {
                capacity: self.capacity,
            });
        }
        Ok(())
    }

    /// Returns the number of slots available in the queue. This many items can
    /// be reserved or pushed before an error is returned.
    fn available_slots(&self) -> usize {
        self.capacity - (self.queue.len() + self.num_slots_reserved)
    }

    /// Reserves a slot if available, else returns `Err(StateError::QueueFull)`.
    fn reserve_slot(&mut self) -> Result<(), StateError> {
        self.check_has_slot()?;
        self.num_slots_reserved += 1;
        Ok(())
    }

    /// Pushes an item into the queue if not full, returns
    /// `Err(StateError::QueueFull)` along with the provided item otherwise.
    fn push(&mut self, msg: T) -> Result<(), (StateError, T)> {
        if let Err(e) = self.check_has_slot() {
            return Err((e, msg));
        }
        self.queue.push_back(msg);
        Ok(())
    }

    /// Pushes an item into a reserved slot, consuming the reservation or
    /// returns an error if there is no reservation available.
    fn push_into_reserved_slot(&mut self, msg: T) -> Result<(), (StateError, T)> {
        if self.num_slots_reserved > 0 {
            self.num_slots_reserved -= 1;
            self.queue.push_back(msg);
            Ok(())
        } else {
            Err((StateError::QueueFull { capacity: 0 }, msg))
        }
    }

    /// Pops an item from the queue. Returns `None` if the queue is empty.
    fn pop(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    /// Returns a reference to the next item in the queue; or `None` if
    /// the queue is empty.
    fn peek(&self) -> Option<&T> {
        self.queue.front()
    }

    /// Returns the number of reserved slots in the queue.
    pub(super) fn reserved_slots(&self) -> usize {
        self.num_slots_reserved
    }

    /// Returns `true` if the queue has one or more used slots.
    pub(super) fn has_used_slots(&self) -> bool {
        !self.queue.is_empty() || self.reserved_slots() > 0
    }

    /// Calculates the sum of the given stat across all enqueued messages.
    ///
    /// Time complexity: O(num_messages).
    fn calculate_stat_sum(&self, stat: impl Fn(&T) -> usize) -> usize {
        self.queue.iter().map(stat).sum::<usize>()
    }
}

impl From<&QueueWithReservation<RequestOrResponse>> for Vec<pb_queues::RequestOrResponse> {
    fn from(q: &QueueWithReservation<RequestOrResponse>) -> Self {
        q.queue.iter().map(|rr| rr.into()).collect()
    }
}

impl From<&QueueWithReservation<Option<RequestOrResponse>>> for Vec<pb_queues::RequestOrResponse> {
    fn from(q: &QueueWithReservation<Option<RequestOrResponse>>) -> Self {
        q.queue
            .iter()
            .map(|opt| match opt {
                Some(rr) => rr.into(),
                None => pb_queues::RequestOrResponse { r: None },
            })
            .collect()
    }
}

/// Validates that the queue capacity is `DEFAULT_QUEUE_CAPACITY`; and that
/// the queue (items plus reservations) is not over capacity.
fn check_size(q: &pb_queues::InputOutputQueue) -> Result<(), ProxyDecodeError> {
    if q.capacity != super::DEFAULT_QUEUE_CAPACITY as u64 {
        return Err(ProxyDecodeError::Other(format!(
            "QueueWithReservation: capacity {}, expecting {}",
            q.capacity,
            super::DEFAULT_QUEUE_CAPACITY
        )));
    }
    if q.capacity < q.queue.len() as u64 + q.num_slots_reserved {
        return Err(ProxyDecodeError::Other(format!(
            "QueueWithReservation: message count ({}) + reserved slots ({}) > capacity ({})",
            q.queue.len(),
            q.num_slots_reserved,
            q.capacity,
        )));
    }
    Ok(())
}

impl TryFrom<pb_queues::InputOutputQueue> for QueueWithReservation<RequestOrResponse> {
    type Error = ProxyDecodeError;

    fn try_from(q: pb_queues::InputOutputQueue) -> Result<Self, Self::Error> {
        check_size(&q)?;
        Ok(QueueWithReservation {
            num_slots_reserved: q.num_slots_reserved as usize,
            capacity: super::DEFAULT_QUEUE_CAPACITY,
            queue: q
                .queue
                .into_iter()
                .map(|rr| rr.try_into())
                .collect::<Result<VecDeque<_>, _>>()?,
        })
    }
}

impl TryFrom<pb_queues::InputOutputQueue> for QueueWithReservation<Option<RequestOrResponse>> {
    type Error = ProxyDecodeError;

    fn try_from(q: pb_queues::InputOutputQueue) -> Result<Self, Self::Error> {
        check_size(&q)?;
        Ok(QueueWithReservation {
            num_slots_reserved: q.num_slots_reserved as usize,
            capacity: super::DEFAULT_QUEUE_CAPACITY,
            queue: q
                .queue
                .into_iter()
                .map(|rr| match rr.r {
                    None => Ok(None),
                    Some(_) => rr.try_into().map(Some),
                })
                .collect::<Result<VecDeque<_>, _>>()?,
        })
    }
}

/// Representation of a single canister input queue. There is an upper bound on
/// the number of messages it can store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct InputQueue {
    queue: QueueWithReservation<RequestOrResponse>,
}

impl InputQueue {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            queue: QueueWithReservation::new(capacity),
        }
    }

    pub(super) fn check_has_slot(&self) -> Result<(), StateError> {
        self.queue.check_has_slot()
    }

    pub(super) fn available_slots(&self) -> usize {
        self.queue.available_slots()
    }

    pub(super) fn push(
        &mut self,
        msg: RequestOrResponse,
    ) -> Result<(), (StateError, RequestOrResponse)> {
        match msg {
            RequestOrResponse::Request(_) => self.queue.push(msg),
            RequestOrResponse::Response(_) => self.queue.push_into_reserved_slot(msg),
        }
    }

    pub fn peek(&self) -> Option<&RequestOrResponse> {
        self.queue.peek()
    }

    pub(super) fn reserve_slot(&mut self) -> Result<(), StateError> {
        self.queue.reserve_slot()
    }

    pub(super) fn pop(&mut self) -> Option<RequestOrResponse> {
        self.queue.pop()
    }

    /// Returns the number of messages in the queue.
    pub(super) fn num_messages(&self) -> usize {
        self.queue.queue.len()
    }

    /// Returns the number of reserved slots in the queue.
    pub(super) fn reserved_slots(&self) -> usize {
        self.queue.reserved_slots()
    }

    /// Returns `true` if the queue has one or more used slots.
    pub(super) fn has_used_slots(&self) -> bool {
        self.queue.has_used_slots()
    }

    /// Returns the amount of cycles contained in the queue.
    pub(super) fn cycles_in_queue(&self) -> Cycles {
        let mut total_cycles = Cycles::zero();
        for msg in self.queue.queue.iter() {
            total_cycles += msg.cycles();
        }
        total_cycles
    }

    /// Calculates the size in bytes, including struct and messages.
    ///
    /// Time complexity: O(num_messages).
    pub(super) fn calculate_size_bytes(&self) -> usize {
        size_of::<Self>() + self.queue.calculate_stat_sum(|msg| msg.count_bytes())
    }

    /// Calculates the sum of the given stat across all enqueued messages.
    ///
    /// Time complexity: O(num_messages).
    pub(super) fn calculate_stat_sum(&self, stat: fn(&RequestOrResponse) -> usize) -> usize {
        self.queue.calculate_stat_sum(stat)
    }
}

impl From<&InputQueue> for pb_queues::InputOutputQueue {
    fn from(q: &InputQueue) -> Self {
        Self {
            queue: (&q.queue).into(),
            begin: 0,
            capacity: q.queue.capacity as u64,
            num_slots_reserved: q.queue.num_slots_reserved as u64,
            deadline_range_ends: Vec::new(),
            timeout_index: 0,
        }
    }
}

impl TryFrom<pb_queues::InputOutputQueue> for InputQueue {
    type Error = ProxyDecodeError;

    fn try_from(q: pb_queues::InputOutputQueue) -> Result<Self, Self::Error> {
        if !q.deadline_range_ends.is_empty() || q.timeout_index != 0 {
            return Err(Self::Error::Other(
                "Found deadlines on decoding InputQueue".to_string(),
            ));
        }
        Ok(Self {
            queue: q.try_into()?,
        })
    }
}

/// Representation of a single Canister output queue.  There is an upper bound
/// on the number of messages it can store. There is also a begin index which
/// can be used effectively as a sequence number for the next message popped out
/// of the queue.
///
/// Uses `Option<_>` items so that requests can be dropped from anywhere in
/// the queue, i.e. replaced with `None`. They will keep their place in the queue
/// until they reach the beginning, where they will be discarded.
///
/// Additionally, an invariant is imposed such that there is always `Some` at the
/// front. This is ensured when a message is popped off the queue by also popping
/// any subsequent `None` items.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct OutputQueue {
    queue: QueueWithReservation<Option<RequestOrResponse>>,
    /// Queue begin index.
    ///
    /// This provides consistent indices that identify each queue item, even as
    /// items are being pushed and popped, for use e.g. in `deadline_range_ends`.
    begin: usize,
    /// Ordered ranges of messages having the same request deadline. Each range
    /// is represented as a deadline and its end index (the index just past
    /// the last request where the deadline applies). Both the deadlines and queue
    /// indices are strictly increasing.
    deadline_range_ends: VecDeque<(Time, usize)>,
    /// Index from which request timing out will resume.
    ///
    /// Used to ensure amortized constant time for timing out requests.
    /// May point before the beginning of the queue if messages have been popped
    /// since the last `time_out_requests()` call.
    timeout_index: usize,
    /// The number of actual messages in the queue.
    num_messages: usize,
}

impl OutputQueue {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            queue: QueueWithReservation::new(capacity),
            begin: 0,
            deadline_range_ends: VecDeque::new(),
            timeout_index: 0,
            num_messages: 0,
        }
    }

    pub(super) fn check_has_slot(&self) -> Result<(), StateError> {
        self.queue.check_has_slot()
    }

    pub(super) fn available_slots(&self) -> usize {
        self.queue.available_slots()
    }

    pub(super) fn push_request(
        &mut self,
        msg: Arc<Request>,
        deadline: Time,
    ) -> Result<(), (StateError, Arc<Request>)> {
        if let Err((err, Some(RequestOrResponse::Request(msg)))) =
            self.queue.push(Some(RequestOrResponse::Request(msg)))
        {
            return Err((err, msg));
        }

        // Update the deadline queue.
        //
        // If the deadline is less than or equal the one at the end of the deadline queue,
        // update the `end` of the last deadline range.
        //
        // If the new deadline is greater than the one of the previous request or there is
        // no previous request in the queue. push a new tuple.
        let end = self.begin + self.queue.queue.len();
        match self.deadline_range_ends.back_mut() {
            Some((back_deadline, deadline_range_end)) if *back_deadline >= deadline => {
                *deadline_range_end = end;
            }
            _ => {
                self.deadline_range_ends.push_back((deadline, end));
            }
        }

        self.num_messages += 1;
        debug_assert!(self.check_invariants());

        Ok(())
    }

    pub(super) fn push_response(&mut self, msg: Arc<Response>) {
        self.queue
            .push_into_reserved_slot(Some(RequestOrResponse::Response(msg)))
            .unwrap();

        self.num_messages += 1;
        debug_assert!(self.check_invariants());
    }

    pub(super) fn reserve_slot(&mut self) -> Result<(), StateError> {
        self.queue.reserve_slot()
    }

    /// Pops a message off the queue and returns it.
    ///
    /// Ensures there is always a 'Some' at the beginning.
    pub(crate) fn pop(&mut self) -> Option<RequestOrResponse> {
        match self.queue.pop() {
            None => None,
            Some(None) => {
                unreachable!("OutputQueue invariant violated: Found `None` at the front.");
            }
            Some(Some(msg)) => {
                self.begin += 1;
                self.advance_to_next_message();

                self.num_messages -= 1;
                debug_assert!(self.check_invariants());

                Some(msg)
            }
        }
    }

    /// Consumes any empty slots at the beginning of the queue and discards consumed deadline ranges.
    fn advance_to_next_message(&mut self) {
        // Remove `None` in the beginning.
        while let Some(None) = self.queue.peek() {
            self.queue.pop();
            self.begin += 1;
        }

        // Remove deadlines that are no longer relevant.
        while let Some((_, deadline_range_end)) = self.deadline_range_ends.front() {
            if *deadline_range_end <= self.begin || *deadline_range_end <= self.timeout_index {
                self.deadline_range_ends.pop_front();
            } else {
                break;
            }
        }
    }

    /// Queue invariant check that panics if any invariant does not hold. Intended
    /// to be called form within a `debug_assert!()` in production code.
    ///
    /// This is (and must remain) strictly a wrapper around `test_invariants()`, as
    /// we should be enforcing the exact same invariants after deserialization as
    /// after mutations.
    ///
    /// # Panics
    ///
    /// If an invariant is violated.
    fn check_invariants(&self) -> bool {
        if let Err(err) = self.test_invariants() {
            panic!("{}", err);
        }
        true
    }

    /// Queue invariant check that produces an error if any invariant does not hold.
    fn test_invariants(&self) -> Result<(), &str> {
        if let Some(None) = self.queue.queue.front() {
            return Err("`None` at the beginning of the queue.");
        }

        if !self
            .deadline_range_ends
            .iter()
            .zip(self.deadline_range_ends.iter().skip(1))
            .all(|(a, b)| a.0 < b.0 && a.1 < b.1)
        {
            return Err("Deadline ranges not sorted.");
        }

        // Deadline indices must be in the
        // `(self.begin, self.begin + self.queue.queue.len()]` interval.
        if let Some((_, first_deadline_range_end)) = self.deadline_range_ends.front() {
            if *first_deadline_range_end <= self.begin {
                return Err("Deadline range end before queue begin.");
            }
            if *first_deadline_range_end <= self.timeout_index {
                return Err("Deadline range end before `timeout_index`.");
            }
        }
        if let Some((_, last_deadline_range_end)) = self.deadline_range_ends.back() {
            if *last_deadline_range_end > self.begin + self.queue.queue.len() {
                return Err("Deadline range end after queue end.");
            }
        }

        if self
            .queue
            .queue
            .iter()
            .take(self.timeout_index.saturating_sub(self.begin))
            .any(|rr| matches!(rr, Some(RequestOrResponse::Request(_))))
        {
            return Err("Request(s) before `timeout_index`.");
        }

        if self.num_messages != self.queue.queue.iter().filter(|rr| rr.is_some()).count() {
            return Err("`num_messages` does is not equal to the number of messages.");
        }

        Ok(())
    }

    /// Returns the message that `pop` would have returned, without removing it
    /// from the queue.
    pub(crate) fn peek(&self) -> Option<&RequestOrResponse> {
        self.queue.peek().map(|msg| msg.as_ref().unwrap())
    }

    /// Number of actual messages in the queue (`None` are ignored).
    pub fn num_messages(&self) -> usize {
        self.num_messages
    }

    /// Returns the number of reserved slots in the queue.
    pub(super) fn reserved_slots(&self) -> usize {
        self.queue.reserved_slots()
    }

    /// Returns `true` if the queue has one or more used slots.
    pub(super) fn has_used_slots(&self) -> bool {
        self.queue.has_used_slots()
    }

    /// Returns the amount of cycles contained in the queue.
    pub(super) fn cycles_in_queue(&self) -> Cycles {
        let mut total_cycles = Cycles::zero();
        for msg in self.queue.queue.iter().flatten() {
            total_cycles += msg.cycles();
        }
        total_cycles
    }

    /// Calculates the sum of the given stat across all enqueued messages.
    ///
    /// Time complexity: O(num_messages).
    pub(super) fn calculate_stat_sum(&self, stat: fn(&RequestOrResponse) -> usize) -> usize {
        let stat =
            |item: &Option<RequestOrResponse>| if let Some(item) = item { stat(item) } else { 0 };
        self.queue.calculate_stat_sum(stat)
    }

    /// Returns true if there are any expired deadlines at `current_time`, false otherwise.
    pub(super) fn has_expired_deadlines(&self, current_time: Time) -> bool {
        match self.deadline_range_ends.front() {
            Some((deadline, _)) => *deadline <= current_time,
            None => false,
        }
    }

    /// Purges timed out requests. Returns an iterator over the timed out requests.
    /// Only consumed items are purged.
    pub(super) fn time_out_requests(&mut self, current_time: Time) -> TimedOutRequestsIter {
        TimedOutRequestsIter {
            q: self,
            current_time,
        }
    }
}

/// Iterator over timed out requests in an OutputQueue.
///
/// This extracts timed out requests by removing them from the queue,
/// leaving `None` in their place and returning them one by one.
pub(super) struct TimedOutRequestsIter<'a> {
    /// A mutable reference to the queue whose requests are to be timed out and returned.
    q: &'a mut OutputQueue,
    /// The time used to determine which requests should be considered timed out.
    /// This is compared to deadlines in q.deadline_range_ends.
    current_time: Time,
}

impl<'a> Iterator for TimedOutRequestsIter<'a> {
    type Item = Arc<Request>;

    fn next(&mut self) -> Option<Self::Item> {
        use RequestOrResponse::Request;

        while let Some(&(deadline, deadline_range_end)) = self.q.deadline_range_ends.front() {
            if deadline > self.current_time {
                return None;
            }

            self.q.timeout_index = self.q.timeout_index.max(self.q.begin);

            debug_assert!(deadline_range_end <= self.q.begin + self.q.queue.queue.len());
            while self.q.timeout_index < deadline_range_end {
                let i = self.q.timeout_index - self.q.begin;
                self.q.timeout_index += 1;

                if let Some(Request(request)) = match self.q.queue.queue.get_mut(i) {
                    Some(item @ Some(Request(_))) => item.take(),
                    _ => continue,
                } {
                    self.q.num_messages -= 1;
                    self.q.advance_to_next_message();
                    debug_assert!(self.q.check_invariants());

                    return Some(request);
                }
            }
            self.q.deadline_range_ends.pop_front();
        }
        None
    }
}

impl std::iter::Iterator for OutputQueue {
    type Item = RequestOrResponse;

    fn next(&mut self) -> Option<Self::Item> {
        self.pop()
    }
}

impl From<&OutputQueue> for pb_queues::InputOutputQueue {
    fn from(q: &OutputQueue) -> Self {
        Self {
            queue: (&q.queue).into(),
            begin: q.begin as u64,
            capacity: q.queue.capacity as u64,
            num_slots_reserved: q.queue.num_slots_reserved as u64,
            deadline_range_ends: q
                .deadline_range_ends
                .iter()
                .map(
                    |(deadline, deadline_range_end)| pb_queues::MessageDeadline {
                        deadline: deadline.as_nanos_since_unix_epoch(),
                        index: *deadline_range_end as u64,
                    },
                )
                .collect(),
            timeout_index: q.timeout_index as u64,
        }
    }
}

impl TryFrom<pb_queues::InputOutputQueue> for OutputQueue {
    type Error = ProxyDecodeError;

    fn try_from(q: pb_queues::InputOutputQueue) -> Result<Self, Self::Error> {
        let begin = q.begin as usize;
        let timeout_index = q.timeout_index as usize;
        let deadline_range_ends: VecDeque<(Time, usize)> = q
            .deadline_range_ends
            .iter()
            .map(|di| {
                (
                    Time::from_nanos_since_unix_epoch(di.deadline),
                    di.index as usize,
                )
            })
            .collect();
        let queue: QueueWithReservation<Option<RequestOrResponse>> = q.try_into()?;
        let num_messages = queue.queue.iter().filter(|rr| rr.is_some()).count();

        let res = Self {
            begin,
            queue,
            deadline_range_ends,
            timeout_index,
            num_messages,
        };

        if let Err(err) = res.test_invariants() {
            return Err(Self::Error::Other(format!("Invalid OutputQueue: {}", err)));
        }
        Ok(res)
    }
}

/// Representation of the Ingress queue.  There is no upper bound on
/// the number of messages it can store.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct IngressQueue {
    queue: VecDeque<Arc<Ingress>>,

    /// Estimated size in bytes.
    size_bytes: usize,
}

impl IngressQueue {
    pub(super) fn push(&mut self, msg: Ingress) {
        self.size_bytes += Self::ingress_size_bytes(&msg);
        self.queue.push_back(Arc::new(msg));
        debug_assert_eq!(Self::size_bytes(&self.queue), self.size_bytes);
    }

    pub(super) fn pop(&mut self) -> Option<Arc<Ingress>> {
        let res = self.queue.pop_front();
        if let Some(msg) = res.as_ref() {
            self.size_bytes -= Self::ingress_size_bytes(msg.as_ref());
            debug_assert_eq!(Self::size_bytes(&self.queue), self.size_bytes);
        }
        res
    }

    pub(super) fn peek(&self) -> Option<&Arc<Ingress>> {
        self.queue.front()
    }

    pub(super) fn size(&self) -> usize {
        self.queue.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Calls `filter` on each ingress message in the queue, retaining the
    /// messages for whom the filter returns `true` and dropping the rest.
    pub(super) fn filter_messages<F>(&mut self, filter: F)
    where
        F: FnMut(&Arc<Ingress>) -> bool,
    {
        self.queue.retain(filter);
        self.size_bytes = Self::size_bytes(&self.queue)
    }

    /// Calculates the size in bytes of an `IngressQueue` holding the given
    /// ingress messages.
    ///
    /// Time complexity: O(num_messages).
    fn size_bytes(queue: &VecDeque<Arc<Ingress>>) -> usize {
        size_of::<Self>()
            + queue
                .iter()
                .map(|i| Self::ingress_size_bytes(i))
                .sum::<usize>()
    }

    /// Returns an estimate of the size of an ingress message in bytes.
    fn ingress_size_bytes(msg: &Ingress) -> usize {
        size_of::<Arc<Ingress>>() + msg.count_bytes()
    }
}

impl Default for IngressQueue {
    fn default() -> Self {
        let queue = Default::default();
        let size_bytes = Self::size_bytes(&queue);
        Self { queue, size_bytes }
    }
}

impl CountBytes for IngressQueue {
    /// Estimate of the queue size in bytes, including metadata.
    fn count_bytes(&self) -> usize {
        self.size_bytes
    }
}

impl From<&IngressQueue> for Vec<pb_ingress::Ingress> {
    fn from(item: &IngressQueue) -> Self {
        item.queue.iter().map(|i| i.as_ref().into()).collect()
    }
}

impl TryFrom<Vec<pb_ingress::Ingress>> for IngressQueue {
    type Error = ProxyDecodeError;

    fn try_from(item: Vec<pb_ingress::Ingress>) -> Result<Self, Self::Error> {
        let queue = item
            .into_iter()
            .map(|i| i.try_into().map(Arc::new))
            .collect::<Result<VecDeque<_>, _>>()?;
        let size_bytes = Self::size_bytes(&queue);

        Ok(IngressQueue { queue, size_bytes })
    }
}
