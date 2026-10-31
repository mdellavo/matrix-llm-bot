use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// One ordered inbox per room, so a room's messages are handled one at a time
/// and each sees the ones before it in history. matrix-sdk runs event handlers
/// concurrently, which would otherwise let a quick follow-up overtake the first.
pub struct RoomQueues<T> {
    rooms: Mutex<HashMap<String, UnboundedSender<T>>>,
}

impl<T> Default for RoomQueues<T> {
    fn default() -> Self {
        Self { rooms: Mutex::new(HashMap::new()) }
    }
}

impl<T> RoomQueues<T> {
    /// Queues `item` for `key`, calling `spawn` with a new inbox if the room has no
    /// live worker: either it's the first message, or the last worker died.
    pub fn enqueue(&self, key: &str, item: T, spawn: impl FnOnce(Inbox<T>)) {
        let mut rooms = self.rooms.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        let item = match rooms.get(key) {
            Some(sender) => match sender.send(item) {
                Ok(()) => return,
                Err(failed) => failed.0,
            },
            None => item,
        };

        let (sender, receiver) = unbounded_channel();
        // Can't fail: the receiver is held right here.
        let _ = sender.send(item);
        rooms.insert(key.to_string(), sender);
        spawn(Inbox { receiver, held: VecDeque::new() });
    }
}

pub struct Inbox<T> {
    receiver: UnboundedReceiver<T>,
    /// Messages already read from the channel but not yet handled.
    held: VecDeque<T>,
}

impl<T> Inbox<T> {
    pub async fn next(&mut self) -> Option<T> {
        match self.held.pop_front() {
            Some(item) => Some(item),
            None => self.receiver.recv().await,
        }
    }

    /// Waits out `window`, then removes the unbroken run of waiting messages that
    /// satisfy `belongs`, stopping at the first that doesn't so order is preserved.
    pub async fn take_burst(&mut self, window: Duration, belongs: impl Fn(&T) -> bool) -> Vec<T> {
        tokio::time::sleep(window).await;

        while let Ok(item) = self.receiver.try_recv() {
            self.held.push_back(item);
        }

        let mut burst = Vec::new();
        while self.held.front().is_some_and(&belongs) {
            burst.extend(self.held.pop_front());
        }
        burst
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inbox_with(items: &[i32]) -> (RoomQueues<i32>, Inbox<i32>) {
        let queues = RoomQueues::default();
        let mut inbox = None;
        for item in items {
            queues.enqueue("room", *item, |created| inbox = Some(created));
        }
        (queues, inbox.expect("first enqueue creates the inbox"))
    }

    #[tokio::test]
    async fn delivers_messages_in_order() {
        let (_queues, mut inbox) = inbox_with(&[1, 2, 3]);
        assert_eq!(inbox.next().await, Some(1));
        assert_eq!(inbox.next().await, Some(2));
        assert_eq!(inbox.next().await, Some(3));
    }

    #[tokio::test]
    async fn take_burst_stops_at_the_first_message_outside_it() {
        let (_queues, mut inbox) = inbox_with(&[1, 2, 9, 3]);
        let burst = inbox.take_burst(Duration::ZERO, |item| *item < 5).await;

        assert_eq!(burst, vec![1, 2]);
        assert_eq!(inbox.next().await, Some(9));
        assert_eq!(inbox.next().await, Some(3));
    }

    #[tokio::test]
    async fn take_burst_is_empty_when_nothing_is_waiting() {
        let (_queues, mut inbox) = inbox_with(&[1]);
        inbox.next().await;
        assert!(inbox.take_burst(Duration::ZERO, |_| true).await.is_empty());
    }

    #[tokio::test]
    async fn a_dead_worker_is_replaced_without_losing_the_message() {
        let (queues, inbox) = inbox_with(&[1]);
        drop(inbox);

        let mut replacement = None;
        queues.enqueue("room", 2, |created| replacement = Some(created));
        assert_eq!(replacement.expect("new inbox").next().await, Some(2));
    }
}
