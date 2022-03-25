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
use futures::{future::BoxFuture, prelude::*, stream::BoxStream};
use libp2p_core::{
    connection::Endpoint,
    // either::EitherOutput,
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    Transport,
};
use std::net::SocketAddr;

// use log::{debug, trace};
// use std::{convert::TryInto, fmt, io, mem, pin::Pin, task::Context, task::Poll};
// use url::Url;

// A WebRTC connection.
pub struct Connection<T> {
    receiver: BoxStream<'static, Result<Incoming, connection::Error>>,
    sender: Pin<Box<dyn Sink<Outgoing, Error = connection::Error> + Send>>,
    _marker: std::marker::PhantomData<T>,
}

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
        let mut inner_addr = addr.clone();

        let proto = match inner_addr.pop() {
            Some(p @ Protocol::XWebRtc(_)) => {
            _ => {
                debug!("{} is not a WebRTC multiaddr", addr);
                return Err(TransportError::MultiaddrNotSupported(addr));
            }
        };

        let transport = self
            .transport
            .listen_on(inner_addr)
            .map_err(|e| e.map(Error::Transport))?;

        let listen = transport
            .map_err(Error::Transport)
            .map_ok(move |event| match event {
                ListenerEvent::NewAddress(mut a) => {
                    a = a.with(proto.clone());
                    debug!("Listening on {}", a);
                    ListenerEvent::NewAddress(a)
                }
                ListenerEvent::AddressExpired(mut a) => {
                    a = a.with(proto.clone());
                    ListenerEvent::AddressExpired(a)
                }
                ListenerEvent::Error(err) => ListenerEvent::Error(Error::Transport(err)),
                ListenerEvent::Upgrade {
                    upgrade,
                    mut local_addr,
                    mut remote_addr,
                } => {
                    local_addr = local_addr.with(proto.clone());
                    remote_addr = remote_addr.with(proto.clone());
                    let remote1 = remote_addr.clone(); // used for logging
                    let remote2 = remote_addr.clone(); // used for logging

                    let upgrade = async move {
                        let stream = upgrade.map_err(Error::Transport).await?;
                        trace!("incoming connection from {}", remote1);

                        // establish WebRTC conn
                        
                        Ok(conn)
                    };

                    ListenerEvent::Upgrade {
                        upgrade: Box::pin(upgrade) as BoxFuture<'static, _>,
                        local_addr,
                        remote_addr,
                    }
                }
            });
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

/// Tries to turn a WebRTC multiaddress into a [`SocketAddr`]. Returns None if the format of the
/// multiaddr is wrong.
fn multiaddr_to_socketaddr(addr: &Multiaddr) -> Option<SocketAddr> {
    let mut iter = addr.iter();
    let proto1 = iter.next()?;
    let proto2 = iter.next()?;
    let proto3 = iter.next()?;

    while let Some(proto) = iter.next() {
        match proto {
            Protocol::P2p(_) => {} // Ignore a `/p2p/...` prefix of possibly outer protocols, if present.
            _ => return None,
        }
    }

    match (proto1, proto2, proto3) {
        (Protocol::Ip4(ip), Protocol::Udp(port), Protocol::P2pWebRtcDirect) => {
            Some(SocketAddr::new(ip.into(), port))
        }
        (Protocol::Ip6(ip), Protocol::Udp(port), Protocol::P2pWebRtcDirect) => {
            Some(SocketAddr::new(ip.into(), port))
        }
        (Protocol::Ip4(ip), Protocol::Tcp(port), Protocol::P2pWebRtcDirect) => {
            Some(SocketAddr::new(ip.into(), port))
        }
        (Protocol::Ip6(ip), Protocol::Tcp(port), Protocol::P2pWebRtcDirect) => {
            Some(SocketAddr::new(ip.into(), port))
        }
        _ => None,
    }
}

/// Turns an IP address and port into the corresponding WebRTC multiaddr.
pub(crate) fn socketaddr_to_multiaddr(
    socket_addr: &SocketAddr,
    transport_protocol: &str,
) -> Multiaddr {
    let p = match transport_protocol {
        "udp" => Protocol::Udp(socket_addr.port()),
        "tcp" => Protocol::Tcp(socket_addr.port()),
        _ => panic!("unsupported protocol: {}", transport_protocol),
    };
    Multiaddr::empty()
        .with(socket_addr.ip().into())
        .with(p)
        .with(Protocol::P2pWebRtcDirect)
}

// Tests //////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn multiaddr_to_socketaddr_conversion() {
        assert!(
            multiaddr_to_socketaddr(&"/ip4/127.0.0.1/udp/1234".parse::<Multiaddr>().unwrap())
                .is_none()
        );

        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/127.0.0.1/tcp/12345/x-webrtc/AC:D1:E5:33:EC:27:1F:CD:E0:27:59:47:F4:D6:2A:2B:23:31:FF:10:C9:DD:E0:29:8E:B7:B3:99:B4:BF:F6:0B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                12345,
            ))
        );

        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/127.0.0.1/tcp/12345/x-webrtc/AC:D1:E5:33:EC:27:1F:CD:E0:27:59:47:F4:D6:2A:2B:23:31:FF:10:C9:DD:E0:29:8E:B7:B3:99:B4:BF:F6:0B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                12345,
            ))
        );

        assert!(multiaddr_to_socketaddr(
            &"/ip4/127.0.0.1/udp/12345/x-webrtc/AC/tcp/12345"
                .parse::<Multiaddr>()
                .unwrap()
        )
        .is_none());

        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/255.255.255.255/udp/8080/x-webrtc/AC:D1:E5:33:EC:27:1F:CD:E0:27:59:47:F4:D6:2A:2B:23:31:FF:10:C9:DD:E0:29:8E:B7:B3:99:B4:BF:F6:0B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
                8080,
            ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/::1/udp/12345/x-webrtc/AC:D1:E5:33:EC:27:1F:CD:E0:27:59:47:F4:D6:2A:2B:23:31:FF:10:C9:DD:E0:29:8E:B7:B3:99:B4:BF:F6:0B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
                12345,
            ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/tcp/8080/x-webrtc/AC:D1:E5:33:EC:27:1F:CD:E0:27:59:47:F4:D6:2A:2B:23:31:FF:10:C9:DD:E0:29:8E:B7:B3:99:B4:BF:F6:0B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(
                    65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535,
                )),
                8080,
            ))
        );
    }

    #[test]
    fn socketaddr_to_multiaddr_conversion() {
        assert_eq!(
            socketaddr_to_multiaddr(
                &SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345,),
                "tcp"
            ),
            "/ip4/127.0.0.1/tcp/12345/x-webrtc/AC:D1:E5:33:EC:27:1F:CD:E0:27:59:47:F4:D6:2A:2B:23:31:FF:10:C9:DD:E0:29:8E:B7:B3:99:B4:BF:F6:0B"
                .parse::<Multiaddr>()
                .unwrap()
        );
    }
}
