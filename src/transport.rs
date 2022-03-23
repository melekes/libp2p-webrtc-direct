// Copyright 2022 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Implementation of the [`Transport`] trait for WebRTC (direct communication without a signaling
//! server).

// use webrtc::peer_connection::RTCPeerConnection;

use crate::error::Error;
use bytes::Bytes;
use futures::{future::BoxFuture, prelude::*, ready, stream::BoxStream};
use libp2p_core::{
    connection::Endpoint,
    either::EitherOutput,
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    PeerId, Transport,
};
// use log::{debug, trace};
// use std::{convert::TryInto, fmt, io, mem, pin::Pin, task::Context, task::Poll};
// use url::Url;

/// A WebRTC transport based on either TCP or UDP transport.
#[derive(Debug, Clone)]
pub struct WebRTCDirectTransport<T> {
    transport: T,
}

impl<T> WebRTCDirectTransport<T> {
    /// Create a new transport based on the inner transport.
    ///
    /// See [`libp2p-tcp`](https://docs.rs/libp2p-tcp/) for constructing the inner transport.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T> Transport for WebRTCDirectTransport<T>
where
    T: Transport + Send + Clone + 'static,
    T::Error: Send + 'static,
    T::Dial: Send + 'static,
    T::Listener: Send + 'static,
    T::ListenerUpgrade: Send + 'static,
    T::Output: Stream + Sink<Bytes> + Unpin + Send + 'static,
{
    type Output = Connection<T::Output>;
    type Error = Error<T::Error>;
    type Listener =
        BoxStream<'static, Result<ListenerEvent<Self::ListenerUpgrade, Self::Error>, Self::Error>>;
    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        Ok(Box::pin(listen))
    }

    fn dial(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.do_dial(addr, Endpoint::Dialer)
    }

    fn dial_as_listener(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.do_dial(addr, Endpoint::Listener)
    }

    fn address_translation(&self, server: &Multiaddr, observed: &Multiaddr) -> Option<Multiaddr> {
        self.transport.address_translation(server, observed)
    }
}

impl<T> WebRTCDirectTransport<T>
where
    T: Transport + Send + Clone + 'static,
    T::Error: Send + 'static,
    T::Dial: Send + 'static,
    T::Listener: Send + 'static,
    T::ListenerUpgrade: Send + 'static,
    T::Output: Stream + Sink<Bytes> + Unpin + Send + 'static,
{
    fn do_dial(
        self,
        addr: Multiaddr,
        role_override: Endpoint,
    ) -> Result<<Self as Transport>::Dial, TransportError<<Self as Transport>::Error>> {
        unimplemented!("TODO")
    }
}

// Tests //////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {}
