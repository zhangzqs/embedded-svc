use core::future::Future;
use core::mem;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

extern crate alloc;
use alloc::borrow::Cow;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::errors::{self, Errors};
use crate::mqtt::client::asyncs::{Client, Connection, Event, MessageId, Publish, QoS};
use crate::mqtt::client::utils::ConnectionState;
use crate::mutex::{Condvar, Mutex, MutexFamily};
use crate::unblocker::asyncs::Unblocker;

pub struct EnqueueFuture<E>(Result<MessageId, E>);

impl<E> Future for EnqueueFuture<E>
where
    E: Clone,
{
    type Output = Result<MessageId, E>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.as_ref() {
            Ok(message_id) => Poll::Ready(Ok(*message_id)),
            Err(err) => Poll::Ready(Err(err.clone())),
        }
    }
}

impl<E> Publish for E
where
    E: crate::mqtt::client::Enqueue,
    E::Error: Clone,
{
    type PublishFuture<'a>
    where
        Self: 'a,
    = EnqueueFuture<E::Error>;

    fn publish<'a, S, V>(
        &'a mut self,
        topic: S,
        qos: QoS,
        retain: bool,
        payload: V,
    ) -> Self::PublishFuture<'a>
    where
        S: Into<Cow<'a, str>>,
        V: Into<Cow<'a, [u8]>>,
    {
        EnqueueFuture(self.enqueue(topic, qos, retain, payload))
    }
}

pub struct AsyncClient<U, M>(Arc<M>, U);

impl<U, M, P> AsyncClient<U, M>
where
    M: Mutex<Data = P>,
{
    pub fn new(unblocker: U, client: P) -> Self {
        Self(Arc::new(M::new(client)), unblocker)
    }
}

impl<U, M, P> Errors for AsyncClient<U, M>
where
    M: Mutex<Data = P>,
    P: Errors,
{
    type Error = P::Error;
}

impl<U, M, P> Clone for AsyncClient<U, M>
where
    U: Clone,
    M: Mutex<Data = P>,
{
    fn clone(&self) -> Self {
        Self(self.0.clone(), self.1.clone())
    }
}

impl<U, M, C> Client for AsyncClient<U, M>
where
    M: Mutex<Data = C> + Send + Sync + 'static,
    C: crate::mqtt::client::Client,
    C::Error: Clone,
    U: Unblocker,
    Self::Error: Send + Sync + 'static,
{
    type SubscribeFuture<'a>
    where
        Self: 'a,
    = U::UnblockFuture<Result<MessageId, C::Error>>;
    type UnsubscribeFuture<'a>
    where
        Self: 'a,
    = U::UnblockFuture<Result<MessageId, C::Error>>;

    fn subscribe<'a, S>(&'a mut self, topic: S, qos: QoS) -> Self::SubscribeFuture<'a>
    where
        S: Into<Cow<'a, str>>,
    {
        let topic: String = topic.into().into_owned();
        let client = self.0.clone();

        self.1.unblock(move || client.lock().subscribe(&topic, qos))
    }

    fn unsubscribe<'a, S>(&'a mut self, topic: S) -> Self::UnsubscribeFuture<'a>
    where
        S: Into<Cow<'a, str>>,
    {
        let topic: String = topic.into().into_owned();
        let client = self.0.clone();

        self.1.unblock(move || client.lock().unsubscribe(&topic))
    }
}

impl<U, M, P> Publish for AsyncClient<U, M>
where
    M: Mutex<Data = P> + Send + Sync + 'static,
    P: crate::mqtt::client::Publish,
    P::Error: Clone,
    U: Unblocker,
    Self::Error: Send + Sync + 'static,
{
    type PublishFuture<'a>
    where
        Self: 'a,
    = U::UnblockFuture<Result<MessageId, P::Error>>;

    fn publish<'a, S, V>(
        &'a mut self,
        topic: S,
        qos: QoS,
        retain: bool,
        payload: V,
    ) -> Self::PublishFuture<'a>
    where
        S: Into<Cow<'a, str>>,
        V: Into<Cow<'a, [u8]>>,
    {
        let topic: String = topic.into().into_owned();
        let payload: Vec<u8> = payload.into().into_owned();
        let client = self.0.clone();

        self.1
            .unblock(move || client.lock().publish(&topic, qos, retain, &payload))
    }
}

impl<U, M, P> crate::utils::asyncify::AsyncWrapper<U, P> for AsyncClient<U, M>
where
    M: Mutex<Data = P>,
{
    fn new(unblocker: U, sync: P) -> Self {
        AsyncClient::new(unblocker, sync)
    }
}

pub enum AsyncState<R, E> {
    None,
    Waiting(Waker),
    Received(Result<Event<R>, E>),
}

impl<R, E> AsyncState<R, E> {
    pub fn new() -> Self {
        Self::None
    }
}

impl<R, E> Default for AsyncState<R, E> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NextFuture<'a, CV, R, E>(&'a ConnectionState<CV, AsyncState<R, E>>)
where
    CV: Condvar + 'a,
    R: 'a,
    E: 'a;

impl<'a, CV, R, E> Future for NextFuture<'a, CV, R, E>
where
    CV: Condvar + 'a,
    R: 'a,
    E: 'a,
{
    type Output = Option<Result<Event<R>, E>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.0.state.lock();

        if let Some(state) = &mut *state {
            let pulled = mem::replace(state, AsyncState::None);

            match pulled {
                AsyncState::Received(event) => {
                    self.0.state_changed.notify_all();

                    Poll::Ready(Some(event))
                }
                _ => {
                    *state = AsyncState::Waiting(cx.waker().clone());
                    self.0.state_changed.notify_all();

                    Poll::Pending
                }
            }
        } else {
            Poll::Ready(None)
        }
    }
}

pub struct AsyncPostbox<CV, R, E>(Arc<ConnectionState<CV, AsyncState<R, E>>>)
where
    CV: Condvar;

impl<CV, R, E> AsyncPostbox<CV, R, E>
where
    CV: Condvar,
    R: Send,
    E: Send,
{
    pub fn new(connection_state: Arc<ConnectionState<CV, AsyncState<R, E>>>) -> Self {
        Self(connection_state)
    }

    pub fn post(&mut self, event: Result<Event<R>, E>) {
        let mut state = self.0.state.lock();

        loop {
            if state.is_none() {
                return;
            } else if matches!(&*state, Some(AsyncState::Received(_))) {
                state = self.0.state_changed.wait(state);
            } else {
                break;
            }
        }

        if let Some(AsyncState::Waiting(waker)) =
            mem::replace(&mut *state, Some(AsyncState::Received(event)))
        {
            waker.wake();
        }
    }
}

pub struct AsyncConnection<CV, R, E>(Arc<ConnectionState<CV, AsyncState<R, E>>>)
where
    CV: Condvar;

impl<CV, R, E> AsyncConnection<CV, R, E>
where
    CV: Condvar,
{
    pub fn new(connection_state: Arc<ConnectionState<CV, AsyncState<R, E>>>) -> Self {
        Self(connection_state)
    }
}

impl<CV, R, E> Drop for AsyncConnection<CV, R, E>
where
    CV: Condvar,
{
    fn drop(&mut self) {
        log::info!("!!!!! About to drop the MQTT async connection");

        self.0.close();

        log::info!("!!!!! The MQTT async connection dropped");
    }
}

impl<CV, R, E> Errors for AsyncConnection<CV, R, E>
where
    CV: Condvar,
    E: errors::Error,
{
    type Error = E;
}

impl<CV, R, E> Connection for AsyncConnection<CV, R, E>
where
    CV: Condvar + Send + Sync + 'static,
    <CV as MutexFamily>::Mutex<Option<AsyncState<R, E>>>: Sync + 'static,
    E: errors::Error,
{
    type Message = R;

    type NextFuture<'a>
    where
        Self: 'a,
        CV: 'a,
        R: 'a,
    = NextFuture<'a, CV, Self::Message, Self::Error>;

    fn next(&mut self) -> Self::NextFuture<'_> {
        NextFuture(&self.0)
    }
}
