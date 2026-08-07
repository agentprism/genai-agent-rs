//! Stream and channel adapters for the assistant event protocol.
//!
//! [`AssistantMessageEventStream`] treats the first terminal event as authoritative, publishes its
//! message to independent result handles, and then behaves as a fused stream.

use crate::{AssistantMessage, AssistantMessageEvent, StopReason, StreamProtocolError};
use futures::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use tokio::sync::{mpsc, watch};

/// Cloneable handle for awaiting an assistant stream's first terminal message.
///
/// A handle observes stream progress but does not drive a pull-based stream. Consume the associated
/// [`AssistantMessageEventStream`] before or concurrently with [`Self::get`], unless a channel
/// producer will publish the terminal event directly.
#[derive(Clone)]
pub struct AssistantMessageResult {
    receiver: watch::Receiver<Option<AssistantMessage>>,
}

impl AssistantMessageResult {
    /// Consume this handle and wait for the first terminal message.
    ///
    /// Other clones remain usable and receive the same message. If every terminal publisher is
    /// dropped before publishing one, this returns [`StreamProtocolError::MissingTerminalEvent`].
    pub async fn get(mut self) -> Result<AssistantMessage, StreamProtocolError> {
        if let Some(message) = self.receiver.borrow().clone() {
            return Ok(message);
        }
        loop {
            self.receiver
                .changed()
                .await
                .map_err(|_| StreamProtocolError::MissingTerminalEvent)?;
            if let Some(message) = self.receiver.borrow().clone() {
                return Ok(message);
            }
        }
    }
}

fn publish_first_terminal(
    sender: &watch::Sender<Option<AssistantMessage>>,
    message: AssistantMessage,
) {
    sender.send_if_modified(|terminal| {
        if terminal.is_some() {
            return false;
        }
        *terminal = Some(message);
        true
    });
}

/// Stream of assistant protocol events plus a final-result handle.
///
/// The first [`AssistantMessageEvent::Done`] or [`AssistantMessageEvent::Error`] is yielded, saved
/// as the result, and permanently fuses this wrapper; the inner stream is never polled again. If
/// the wrapper reaches end-of-stream or is dropped without a terminal event, its result handles
/// report [`StreamProtocolError::MissingTerminalEvent`] once no other publisher remains.
///
/// Wrapping a pull-driven stream does not create a background consumer. Calling [`Self::result`]
/// alone therefore cannot advance that stream to its terminal event.
pub struct AssistantMessageEventStream {
    inner: Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send + 'static>>,
    terminal_sender: Option<watch::Sender<Option<AssistantMessage>>>,
    result: AssistantMessageResult,
    terminated: bool,
}

impl std::fmt::Debug for AssistantMessageEventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantMessageEventStream")
            .finish_non_exhaustive()
    }
}

impl AssistantMessageEventStream {
    /// Wrap a pull-driven event stream without spawning or eagerly polling it.
    pub fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = AssistantMessageEvent> + Send + 'static,
    {
        let (terminal_sender, receiver) = watch::channel(None);
        Self {
            inner: Box::pin(stream),
            terminal_sender: Some(terminal_sender),
            result: AssistantMessageResult { receiver },
            terminated: false,
        }
    }

    /// Build a stream that yields the supplied events in order.
    pub fn from_events(events: Vec<AssistantMessageEvent>) -> Self {
        Self::from_stream(futures::stream::iter(events))
    }

    /// Build a two-event stream from an already assembled message.
    ///
    /// The stream first emits [`AssistantMessageEvent::Start`] with the stop reason reset to
    /// [`StopReason::Pending`]. It then emits `Error` for `Error`/`Aborted` messages and `Done` for
    /// every other stop reason.
    pub fn from_message(message: AssistantMessage) -> Self {
        let terminal = if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            AssistantMessageEvent::Error {
                reason: message.stop_reason,
                error: message.clone(),
            }
        } else {
            AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message: message.clone(),
            }
        };
        let mut partial = message;
        partial.stop_reason = StopReason::Pending;
        Self::from_events(vec![AssistantMessageEvent::Start { partial }, terminal])
    }

    /// Build a stream from a preassembled failure message.
    ///
    /// This is a descriptive alias for [`Self::from_message`]; callers should set the message's
    /// stop reason to [`StopReason::Error`] or [`StopReason::Aborted`].
    pub fn from_error(message: AssistantMessage) -> Self {
        Self::from_message(message)
    }

    /// Create an unbounded, channel-backed stream and its cloneable producer.
    ///
    /// Sending does not apply backpressure, so producers that can outpace consumption should bound
    /// their own update rate. A terminal send publishes the result immediately, without requiring
    /// the event stream to be polled.
    pub fn channel() -> (AssistantStreamSender, Self) {
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let (terminal_sender, receiver) = watch::channel(None);
        let producer_terminal = terminal_sender.clone();
        let completed = Arc::new(Mutex::new(false));
        let stream = futures::stream::poll_fn(move |cx| event_receiver.poll_recv(cx));
        (
            AssistantStreamSender {
                event_sender,
                terminal_sender: producer_terminal,
                completed,
            },
            Self {
                inner: Box::pin(stream),
                terminal_sender: Some(terminal_sender),
                result: AssistantMessageResult { receiver },
                terminated: false,
            },
        )
    }

    /// Clone a handle that observes this stream's first terminal message.
    pub fn result_handle(&self) -> AssistantMessageResult {
        self.result.clone()
    }

    /// Await the first terminal message without consuming the stream object.
    ///
    /// For pull-driven streams, call this after or concurrently with consuming events so the inner
    /// stream continues to make progress.
    pub async fn result(&self) -> Result<AssistantMessage, StreamProtocolError> {
        self.result.clone().get().await
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }

        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(event)) => {
                if let Some(message) = event.terminal_message() {
                    if let Some(sender) = self.terminal_sender.take() {
                        publish_first_terminal(&sender, message.clone());
                    }
                    self.terminated = true;
                }
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => {
                self.terminal_sender.take();
                self.terminated = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Sending failed because the assistant event-stream receiver was dropped.
#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
#[error("assistant event stream receiver is closed")]
pub struct AssistantStreamSendError;

/// Producer half of a channel-backed [`AssistantMessageEventStream`].
#[derive(Clone)]
pub struct AssistantStreamSender {
    event_sender: mpsc::UnboundedSender<AssistantMessageEvent>,
    terminal_sender: watch::Sender<Option<AssistantMessage>>,
    completed: Arc<Mutex<bool>>,
}

impl AssistantStreamSender {
    /// Enqueue an event, serializing terminal settlement across all sender clones.
    ///
    /// The first terminal event is enqueued and published to result handles atomically with respect
    /// to competing sends. Later sends are ignored and return `Ok(())` because the protocol is
    /// already settled.
    ///
    /// # Errors
    ///
    /// Returns [`AssistantStreamSendError`] if the stream receiver was dropped before this event
    /// could be accepted.
    pub fn send(&self, event: AssistantMessageEvent) -> Result<(), AssistantStreamSendError> {
        let mut completed = self
            .completed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *completed {
            return Ok(());
        }

        let terminal = event.terminal_message().cloned();
        self.event_sender
            .send(event)
            .map_err(|_| AssistantStreamSendError)?;
        if let Some(message) = terminal {
            *completed = true;
            publish_first_terminal(&self.terminal_sender, message);
        }
        Ok(())
    }

    /// Return `true` after a terminal event was accepted or the stream receiver was dropped.
    pub fn is_closed(&self) -> bool {
        *self
            .completed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            || self.event_sender.is_closed()
    }
}
