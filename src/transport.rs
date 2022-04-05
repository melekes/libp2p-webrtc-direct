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

use libp2p_core::{
    address_translation,
    // connection::Endpoint,
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    Transport,
};

// use bytes::Bytes;
use futures::{future::BoxFuture, prelude::*, ready, stream::BoxStream};
use futures_timer::Delay;
use if_watch::{IfEvent, IfWatcher};
use log::{debug, error, trace};
use tinytemplate::TinyTemplate;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::peer_connection::certificate::RTCCertificate;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc_ice::udp_mux::UDPMux;
use webrtc_ice::udp_mux::UDPMuxDefault;
use webrtc_ice::udp_mux::UDPMuxParams;
use webrtc_ice::udp_network::UDPNetwork;

#[cfg(feature = "tokio")]
use tokio_crate::net::{ToSocketAddrs, UdpSocket};

#[cfg(feature = "async-std")]
use async_std_crate::net::{ToSocketAddrs, UdpSocket};

use std::borrow::Cow;
use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::error::Error;
use crate::sdp;

enum IfWatch {
    Pending(BoxFuture<'static, io::Result<IfWatcher>>),
    Ready(IfWatcher),
}

/// The listening addresses of a [`WebRTCDirectTransport`].
enum InAddr {
    /// The stream accepts connections on a single interface.
    One {
        addr: IpAddr,
        out: Option<Multiaddr>,
    },
    /// The stream accepts connections on all interfaces.
    Any { if_watch: IfWatch },
}

// A WebRTC connection.
pub struct Connection {
    connection: RTCPeerConnection,
    // receiver: BoxStream<'static, Result<Incoming, connection::Error>>,
    // sender: Pin<Box<dyn Sink<Outgoing, Error = connection::Error> + Send>>,
}

/// A WebRTC direct transport <https://webrtc.rs/> webrtc-rs library.
pub struct WebRTCDirectTransport {
    config: RTCConfiguration,
    udp_mux: Arc<dyn UDPMux + Send + Sync>,
}

impl WebRTCDirectTransport {
    /// Create a new WebRTC transport.
    ///
    /// Creates a UDP socket bound to `addr`.
    pub async fn new<A: ToSocketAddrs>(
        certificate: RTCCertificate,
        addr: A,
    ) -> Result<Self, TransportError<Error>> {
        // Create a webrtc config with the given certificate, which will be used for all DTLS
        // handshakes.
        let config = RTCConfiguration {
            certificates: vec![certificate],
            ..Default::default()
        };

        // Create a UDP mux, which will be used across all connections.
        let socket = UdpSocket::bind(addr)
            .map_err(Error::IoError)
            .map_err(TransportError::Other)
            .await?;
        let udp_mux = UDPMuxDefault::new(UDPMuxParams::new(socket));

        Ok(Self { config, udp_mux })
    }
}

impl Transport for WebRTCDirectTransport {
    type Output = Connection;
    type Error = Error;
    type Listener = WebRTCListenStream;
    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        let socket_addr = multiaddr_to_socketaddr(&addr)
            .ok_or_else(|| TransportError::MultiaddrNotSupported(addr))?;
        log::debug!("listening on {}", socket_addr);

        Ok(WebRTCListenStream::new(socket_addr))
    }

    fn dial(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        Ok(Box::pin(self.do_dial(addr)))
    }

    fn dial_as_listener(self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.dial(addr)
    }

    fn address_translation(&self, server: &Multiaddr, observed: &Multiaddr) -> Option<Multiaddr> {
        address_translation(server, observed)
    }
}

/// A stream of incoming connections on one or more interfaces.
pub struct WebRTCListenStream {
    /// The socket address that the listening socket is bound to,
    /// which may be a "wildcard address" like `INADDR_ANY` or `IN6ADDR_ANY`
    /// when listening on all interfaces for IPv4 respectively IPv6 connections.
    listen_addr: SocketAddr,
    /// The IP addresses of network interfaces on which the listening socket
    /// is accepting connections.
    ///
    /// If the listen socket listens on all interfaces, these may change over
    /// time as interfaces become available or unavailable.
    in_addr: InAddr,
    /// How long to sleep after a (non-fatal) error while trying
    /// to accept a new connection.
    sleep_on_error: Duration,
    /// The current pause, if any.
    pause: Option<Delay>,
}

impl WebRTCListenStream {
    /// Constructs a `WebRTCListenStream` for incoming connections around
    /// the given `TcpListener`.
    fn new(listen_addr: SocketAddr) -> Self {
        // Check whether the listening IP is set or not.
        let in_addr = if match &listen_addr {
            SocketAddr::V4(a) => a.ip().is_unspecified(),
            SocketAddr::V6(a) => a.ip().is_unspecified(),
        } {
            // The `addrs` are populated via `if_watch` when the
            // `WebRTCDirectTransport` is polled.
            InAddr::Any {
                if_watch: IfWatch::Pending(IfWatcher::new().boxed()),
            }
        } else {
            InAddr::One {
                out: Some(ip_to_multiaddr(listen_addr.ip(), listen_addr.port())),
                addr: listen_addr.ip(),
            }
        };

        WebRTCListenStream {
            listen_addr,
            in_addr,
            pause: None,
            sleep_on_error: Duration::from_millis(100),
        }
    }
}

impl Stream for WebRTCListenStream {
    type Item = Result<ListenerEvent<BoxFuture<'static, Result<Connection, Error>>, Error>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let me = Pin::into_inner(self);

        loop {
            match &mut me.in_addr {
                InAddr::Any { if_watch } => match if_watch {
                    // If we listen on all interfaces, wait for `if-watch` to be ready.
                    IfWatch::Pending(f) => match ready!(Pin::new(f).poll(cx)) {
                        Ok(w) => {
                            *if_watch = IfWatch::Ready(w);
                            continue;
                        },
                        Err(err) => {
                            log::debug! {
                                "Failed to begin observing interfaces: {:?}. Scheduling retry.",
                                err
                            };
                            *if_watch = IfWatch::Pending(IfWatcher::new().boxed());
                            me.pause = Some(Delay::new(me.sleep_on_error));
                            return Poll::Ready(Some(Ok(ListenerEvent::Error(Error::IoError(
                                err,
                            )))));
                        },
                    },
                    // Consume all events for up/down interface changes.
                    IfWatch::Ready(watch) => {
                        while let Poll::Ready(ev) = watch.poll_unpin(cx) {
                            match ev {
                                Ok(IfEvent::Up(inet)) => {
                                    let ip = inet.addr();
                                    if me.listen_addr.is_ipv4() == ip.is_ipv4()
                                        || me.listen_addr.is_ipv6() == ip.is_ipv6()
                                    {
                                        // TODO: include fingerprint?
                                        let ma = ip_to_multiaddr(ip, me.listen_addr.port());
                                        log::debug!("New listen address: {}", ma);
                                        return Poll::Ready(Some(Ok(ListenerEvent::NewAddress(
                                            ma,
                                        ))));
                                    }
                                },
                                Ok(IfEvent::Down(inet)) => {
                                    let ip = inet.addr();
                                    if me.listen_addr.is_ipv4() == ip.is_ipv4()
                                        || me.listen_addr.is_ipv6() == ip.is_ipv6()
                                    {
                                        // TODO: include fingerprint?
                                        let ma = ip_to_multiaddr(ip, me.listen_addr.port());
                                        log::debug!("Expired listen address: {}", ma);
                                        return Poll::Ready(Some(Ok(
                                            ListenerEvent::AddressExpired(ma),
                                        )));
                                    }
                                },
                                Err(err) => {
                                    log::debug! {
                                        "Failure polling interfaces: {:?}. Scheduling retry.",
                                        err
                                    };
                                    me.pause = Some(Delay::new(me.sleep_on_error));
                                    return Poll::Ready(Some(Ok(ListenerEvent::Error(
                                        Error::IoError(err),
                                    ))));
                                },
                            }
                        }
                    },
                },
                // If the listener is bound to a single interface, make sure the
                // address is registered for port reuse and reported once.
                InAddr::One { addr, out } => {
                    if let Some(multiaddr) = out.take() {
                        return Poll::Ready(Some(Ok(ListenerEvent::NewAddress(multiaddr))));
                    }
                },
            }
            // TODO: get new connection event from an endpoint? and establish new connection
        }
    }
}

// async fn upgrade() {
//     let mut se = build_settings(self.udp_mux.clone());
//     // Act as a DTLS server (one which waits for a connection).
//     se.set_answering_dtls_role(DTLSRole::Server)?;

//     let api = APIBuilder::new().with_setting_engine(se).build();
//     let peer_connection = api.new_peer_connection(config).await?;

//     peer_connection
//         .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
//             if s != RTCPeerConnectionState::Failed {
//                 debug!("Peer Connection State has changed: {}", s);
//             } else {
//                 // Wait until PeerConnection has had no network activity for 30 seconds or another
//                 // failure. It may be reconnected using an ICE Restart. Use
//                 // webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster
//                 // timeout. Note that the PeerConnection may come back from
//                 // PeerConnectionStateDisconnected.
//                 error!("Peer Connection has gone to failed => exiting");
//                 // TODO: stop listening?
//             }

//             Box::pin(async {})
//         }))
//         .await;

//     peer_connection
//         .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
//             let d_label = d.label().to_owned();
//             let d_id = d.id();
//             debug!("New DataChannel {} {}", d_label, d_id);

//             // Register channel opening handling
//             Box::pin(async move {
//                 // let d2 = Arc::clone(&d);
//                 let d_label2 = d_label.clone();
//                 let d_id2 = d_id;
//                 d.on_open(Box::new(move || {
//                     debug!("Data channel '{}'-'{}' open", d_label2, d_id2);
//                     Box::pin(async {})
//                 }))
//                 .await;

//                 // Register text message handling
//                 d.on_message(Box::new(move |msg: DataChannelMessage| {
//                     let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
//                     debug!("Message from DataChannel '{}': '{}'", d_label, msg_str);
//                     Box::pin(async {})
//                 }))
//                 .await;
//             })
//         }))
//         .await;

//     // Set the remote description to the predefined SDP
//     let mut offer = peer_connection.create_offer(None).await?;
//     offer.sdp = sdp::CLIENT_SESSION_DESCRIPTION.to_string();
//     debug!("REMOTE OFFER: {:?}", offer);
//     peer_connection.set_remote_description(offer).await?;

//     let answer = peer_connection.create_answer(None).await?;
//     // Set the local description and start UDP listeners
//     // Note: this will start the gathering of ICE candidates
//     debug!("LOCAL ANSWER: {:?}", answer);
//     peer_connection.set_local_description(answer).await?;

//     Ok(Connection {
//         connection: peer_connection,
//     })
// }

impl WebRTCDirectTransport {
    async fn do_dial(self, addr: Multiaddr) -> Result<Connection, Error> {
        log::debug!("dialing {}", addr);
        let mut inner_addr = addr.clone();

        let fingerprint = match inner_addr.pop() {
            Some(Protocol::XWebRTC(f)) => f,
            _ => {
                debug!("{} is not a WebRTC multiaddr", addr);
                return Err(Error::InvalidMultiaddr(addr));
            },
        };

        let socket_addr = multiaddr_to_socketaddr(&inner_addr)
            .ok_or_else(|| Error::InvalidMultiaddr(addr.clone()))?;
        if socket_addr.port() == 0 || socket_addr.ip().is_unspecified() {
            return Err(Error::InvalidMultiaddr(addr.clone()));
        }

        let server_session_description = {
            let mut tt = TinyTemplate::new();
            tt.add_template("description", sdp::SERVER_SESSION_DESCRIPTION)
                .unwrap();

            let context = sdp::SessionDescriptionContext {
                ip_version: {
                    if socket_addr.is_ipv4() {
                        sdp::IpVersion::IP4
                    } else {
                        sdp::IpVersion::IP6
                    }
                },
                target_ip: socket_addr.ip(),
                target_port: socket_addr.port(),
                fingerprint: hex::encode(fingerprint.as_ref()),
            };
            tt.render("description", &context).unwrap()
        };

        let config = self.config.clone();

        let remote = addr.clone(); // used for logging

        trace!("dialing address: {:?}", remote);

        let mut se = build_settings(self.udp_mux.clone());

        // Act as a DTLS client (one which initiates a connection).
        se.set_answering_dtls_role(DTLSRole::Client)
            .map_err(Error::WebRTC)?;

        let api = APIBuilder::new().with_setting_engine(se).build();

        let peer_connection = api
            .new_peer_connection(config)
            .map_err(Error::WebRTC)
            .await?;

        // TODO: dedup
        peer_connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                if s != RTCPeerConnectionState::Failed {
                    debug!("Peer Connection State has changed: {}", s);
                } else {
                    // Wait until PeerConnection has had no network activity for 30 seconds or another
                    // failure. It may be reconnected using an ICE Restart. Use
                    // webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster
                    // timeout. Note that the PeerConnection may come back from
                    // PeerConnectionStateDisconnected.
                    error!("Peer Connection has gone to failed => exiting");
                    // TODO: stop listening?
                }

                Box::pin(async {})
            }))
            .await;

        peer_connection
            .on_data_channel(Box::new(move |d: Arc<RTCDataChannel>| {
                let d_label = d.label().to_owned();
                let d_id = d.id();
                debug!("New DataChannel {} {}", d_label, d_id);

                // Register channel opening handling
                Box::pin(async move {
                    // let d2 = Arc::clone(&d);
                    let d_label2 = d_label.clone();
                    let d_id2 = d_id;
                    d.on_open(Box::new(move || {
                        debug!("Data channel '{}'-'{}' open", d_label2, d_id2);
                        Box::pin(async {})
                    }))
                    .await;

                    // Register text message handling
                    d.on_message(Box::new(move |msg: DataChannelMessage| {
                        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
                        debug!("Message from DataChannel '{}': '{}'", d_label, msg_str);
                        Box::pin(async {})
                    }))
                    .await;
                })
            }))
            .await;

        let offer = peer_connection
            .create_offer(None)
            .map_err(Error::WebRTC)
            .await?;
        debug!("LOCAL OFFER: {:?}", offer);
        peer_connection
            .set_local_description(offer)
            .map_err(Error::WebRTC)
            .await?;

        let mut answer = peer_connection
            .create_answer(None)
            .map_err(Error::WebRTC)
            .await?;
        // Set the local description and start UDP listeners
        // Note: this will start the gathering of ICE candidates
        answer.sdp = server_session_description;
        debug!("REMOTE ANSWER: {:?}", answer);
        peer_connection
            .set_remote_description(answer)
            .map_err(Error::WebRTC)
            .await?;

        Ok(Connection {
            connection: peer_connection,
        })
    }
}

// Create a [`Multiaddr`] from the given IP address and port number.
fn ip_to_multiaddr(ip: IpAddr, port: u16) -> Multiaddr {
    Multiaddr::empty().with(ip.into()).with(Protocol::Udp(port))
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
            Protocol::P2p(_) => {}, // Ignore a `/p2p/...` prefix of possibly outer protocols, if present.
            _ => return None,
        }
    }

    match (proto1, proto2, proto3) {
        (Protocol::Ip4(ip), Protocol::Udp(port), Protocol::XWebRTC(_)) => {
            Some(SocketAddr::new(ip.into(), port))
        },
        (Protocol::Ip6(ip), Protocol::Udp(port), Protocol::XWebRTC(_)) => {
            Some(SocketAddr::new(ip.into(), port))
        },
        (Protocol::Ip4(ip), Protocol::Tcp(port), Protocol::XWebRTC(_)) => {
            Some(SocketAddr::new(ip.into(), port))
        },
        (Protocol::Ip6(ip), Protocol::Tcp(port), Protocol::XWebRTC(_)) => {
            Some(SocketAddr::new(ip.into(), port))
        },
        _ => None,
    }
}

/// Turns an IP address and port into the corresponding WebRTC multiaddr.
pub(crate) fn socketaddr_to_multiaddr<'a>(
    socket_addr: &SocketAddr,
    fingerprint: Cow<'a, [u8; 32]>,
) -> Multiaddr {
    Multiaddr::empty()
        .with(socket_addr.ip().into())
        .with(Protocol::Udp(socket_addr.port()))
        .with(Protocol::XWebRTC(fingerprint))
}

fn build_settings(udp_mux: Arc<dyn UDPMux + Send + Sync>) -> SettingEngine {
    let mut se = SettingEngine::default();

    // Disable remote's fingerprint verification.
    se.disable_certificate_fingerprint_verification(true);

    // Act as a lite ICE (ICE which does not send additional candidates).
    se.set_lite(true);

    // Set both ICE user and password to fingerprint.
    // It will be checked by remote side when exchanging ICE messages.
    se.set_ice_credentials("user".to_string(), "password".to_string());

    se.set_udp_network(UDPNetwork::Muxed(udp_mux));

    se
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
                &"/ip4/127.0.0.1/tcp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(( SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                        12345,
            ) ))
        );

        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/127.0.0.1/tcp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(( SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                        12345,
            ) ))
        );

        assert!(multiaddr_to_socketaddr(
            &"/ip4/127.0.0.1/udp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B/tcp/12345"
                .parse::<Multiaddr>()
                .unwrap()
        )
        .is_none());

        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/255.255.255.255/udp/8080/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(( SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
                        8080,
            ) ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/::1/udp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(( SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
                        12345,
            ) ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/tcp/8080/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some(( SocketAddr::new(
                        IpAddr::V6(Ipv6Addr::new(
                                65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535,
                        )),
                        8080,
            ) ))
        );
    }

    fn hex_to_cow<'a>(s: &str) -> Cow<'a, [u8; 32]> {
        let mut buf = [0; 32];
        hex::decode_to_slice(s, &mut buf).unwrap();
        Cow::Owned(buf)
    }

    #[test]
    fn socketaddr_to_multiaddr_conversion() {
        assert_eq!(
            socketaddr_to_multiaddr(
                &SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345,),
                hex_to_cow("ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"),
            ),
            "/ip4/127.0.0.1/tcp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                .parse::<Multiaddr>()
                .unwrap()
        );
    }
}
