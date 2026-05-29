//! Ring buffer channel implementation for efficient resource reuse.
//!
//! This module provides a ring buffer channel that allows for efficient
//! reuse of resources by maintaining a fixed pool of items that circulate
//! between a producer and consumer. This is particularly useful for
//! managing buffers or other reusable resources.

use crossbeam::channel::{unbounded, RecvError, SendError};

/// Producer end of a ring buffer channel.
///
/// The producer can:
/// - Send new items to the consumer
/// - Receive returned items from the consumer
pub struct Producer<T: Clone> {
    /// Channel for sending items to the consumer
    fwd_sender: crossbeam::channel::Sender<T>,
    /// Channel for receiving returned items from the consumer
    bck_receiver: crossbeam::channel::Receiver<T>,
}

/// Consumer end of a ring buffer channel.
///
/// The consumer can:
/// - Receive items from the producer
/// - Return items back to the producer
pub struct Consumer<T: Clone> {
    /// Channel for returning items to the producer
    bck_sender: crossbeam::channel::Sender<T>,
    /// Channel for receiving items from the producer
    fwd_receiver: crossbeam::channel::Receiver<T>,
}

/// Creates a new ring buffer channel with a specified size and initial value.
///
/// # Arguments
/// * `size` - Number of items to pre-allocate in the channel
/// * `t` - Initial value to clone for each slot
///
/// # Returns
/// A tuple containing the producer and consumer ends of the channel
pub fn new<T: Clone>(size: usize, t: &T) -> (Producer<T>, Consumer<T>) {
    let (fwd_sender, fwd_receiver) = unbounded();
    let (bck_sender, bck_receiver) = unbounded();
    for _ in 0..size {
        bck_sender.send(t.clone()).unwrap();
    }
    let prod = Producer {
        fwd_sender,
        bck_receiver,
    };
    let cons = Consumer {
        bck_sender,
        fwd_receiver,
    };
    (prod, cons)
}

impl<T: Clone> Producer<T> {
    /// Sends an item to the consumer.
    ///
    /// # Arguments
    /// * `t` - Item to send
    ///
    /// # Returns
    /// Ok(()) if successful, or SendError if the channel is disconnected
    pub fn produce(&mut self, t: T) -> Result<(), SendError<T>> {
        self.fwd_sender.send(t)
    }

    /// Receives a returned item from the consumer.
    ///
    /// # Returns
    /// Ok(T) if an item was received, or RecvError if the channel is disconnected
    pub fn receive_returned(&mut self) -> Result<T, RecvError> {
        self.bck_receiver.recv()
    }
}

impl<T: Clone> Consumer<T> {
    /// Receives an item from the producer.
    ///
    /// # Returns
    /// The received item
    ///
    /// # Panics
    /// If the channel is disconnected
    pub fn consume(&mut self) -> T {
        self.fwd_receiver.recv().unwrap()
    }

    /// Returns an item back to the producer.
    ///
    /// # Arguments
    /// * `t` - Item to return
    ///
    /// # Panics
    /// If the channel is disconnected
    pub fn send_returned(&mut self, t: T) {
        self.bck_sender.send(t).unwrap();
    }
}
