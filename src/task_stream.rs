use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::{wrappers::ReceiverStream, Stream};

/// A struct for spawning tasks and streaming their return values as they
/// complete
pub struct TaskStream<T> {
    sender: Sender<T>,
    receiver: Receiver<T>,
    empty: bool,
}

impl<T> TaskStream<T> {
    /// Create a new `TaskStream`
    ///
    /// `buffer` is the message queue size to use for the internal mpsc
    /// channel.
    pub fn new(buffer: usize) -> Self {
        let (sender, receiver) = channel(buffer);
        TaskStream {
            sender,
            receiver,
            empty: true,
        }
    }

    /// Test whether any tasks have been spawned in this `TaskStream`
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    /// Convert the `TaskStream` into a stream of its spawned tasks' return
    /// values
    pub fn into_stream(self) -> impl Stream<Item = T> {
        drop(self.sender);
        ReceiverStream::new(self.receiver)
    }
}

impl<T: 'static> TaskStream<T> {
    /// Spawn a task to return the output of via the stream
    pub fn spawn<F>(&mut self, fut: F)
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send,
    {
        let sender = self.sender.clone();
        tokio::spawn(async move { sender.send(fut.await).await });
        self.empty = false;
    }
}
