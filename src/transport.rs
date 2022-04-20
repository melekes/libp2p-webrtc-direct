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

// What's left:
// - [ ] make sure ufrag / psw are correct
// - [ ] noise handshake on top of data channel
// - [ ] peer ID

use libp2p_core::{
    address_translation,
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    Transport,
};

use futures::{
    channel::{mpsc, oneshot},
    future::BoxFuture,
    prelude::*,
    ready,
};
use futures_timer::Delay;
use if_watch::{IfEvent, IfWatcher};
use log::{debug, error, trace};
use tinytemplate::TinyTemplate;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::peer_connection::certificate::RTCCertificate;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc_data::data_channel::DataChannel as DetachedDataChannel;
use webrtc_ice::udp_mux::UDPMux;
use webrtc_ice::udp_network::UDPNetwork;

use tokio_crate::net::{ToSocketAddrs, UdpSocket};

use std::borrow::Cow;
use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use crate::error::Error;
use crate::sdp;

use crate::connection::Connection;
use crate::udp_mux::UDPMuxNewAddr;
use crate::udp_mux::UDPMuxParams;
use crate::upgrade::WebRTCUpgrade;

enum IfWatch {
    Pending(BoxFuture<'static, io::Result<IfWatcher>>),
    Ready(IfWatcher),
}

/// The listening addresses of a [`WebRTCDirectTransport`].
enum InAddr {
    /// The stream accepts connections on a single interface.
    One { out: Option<Multiaddr> },
    /// The stream accepts connections on all interfaces.
    Any { if_watch: IfWatch },
}

/// A WebRTC direct transport.
#[derive(Clone)]
pub struct WebRTCDirectTransport {
    /// A `RTCConfiguration` which holds this peer's certificate(s).
    config: RTCConfiguration,

    /// The `UDPMux` that manages all ICE connections.
    udp_mux: Arc<dyn UDPMux + Send + Sync>,

    /// The local address of `udp_mux`.
    udp_mux_addr: SocketAddr,

    /// The receiver for new `SocketAddr` connecting to this peer.
    new_addr_rx: Arc<Mutex<mpsc::Receiver<SocketAddr>>>,
}

impl WebRTCDirectTransport {
    /// Create a new WebRTC transport.
    ///
    /// Creates a UDP socket bound to `listen_addr`. `listen_on` must be called to start listening.
    pub async fn new<A: ToSocketAddrs>(
        certificate: RTCCertificate,
        listen_addr: A,
    ) -> Result<Self, TransportError<Error>> {
        // bind to `listen_addr` and construct a UDP mux.
        let socket = UdpSocket::bind(listen_addr)
            .map_err(Error::IoError)
            .map_err(TransportError::Other)
            .await?;
        let udp_mux_addr = socket
            .local_addr()
            .map_err(Error::IoError)
            .map_err(TransportError::Other)?;
        let (new_addr_tx, new_addr_rx) = mpsc::channel(1);
        let udp_mux = UDPMuxNewAddr::new(UDPMuxParams::new(socket), new_addr_tx);

        Ok(Self {
            config: RTCConfiguration {
                certificates: vec![certificate],
                ..Default::default()
            },
            udp_mux,
            udp_mux_addr,
            new_addr_rx: Arc::new(Mutex::new(new_addr_rx)),
        })
    }

    /// Returns the SHA-256 fingerprint of the certificate in lowercase hex string as expressed
    /// utilizing the syntax of 'fingerprint' in <https://tools.ietf.org/html/rfc4572#section-5>.
    fn cert_fingerprint(&self) -> String {
        // safe to unwrap here because we require a certificate above
        let fingerprints = self
            .config
            .certificates
            .first()
            .expect("at least one certificate")
            .get_fingerprints()
            .expect("fingerprints to succeed");
        fingerprints.first().unwrap().value.to_owned()
    }
}

impl Transport for WebRTCDirectTransport {
    type Output = Connection<'static>;
    type Error = Error;
    type Listener = WebRTCListenStream;
    type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn listen_on(self, addr: Multiaddr) -> Result<Self::Listener, TransportError<Self::Error>> {
        log::debug!("listening on {} (ignoring {})", self.udp_mux_addr, addr);
        Ok(WebRTCListenStream::new(
            self.udp_mux_addr,
            self.config.clone(),
            self.udp_mux.clone(),
            self.new_addr_rx.clone(),
        ))
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

    /// A `RTCConfiguration` which holds this peer's certificate(s).
    config: RTCConfiguration,
    /// The `UDPMux` that manages all ICE connections.
    udp_mux: Arc<dyn UDPMux + Send + Sync>,
    /// The receiver for new `SocketAddr` connecting to this peer.
    new_addr_rx: Arc<Mutex<mpsc::Receiver<SocketAddr>>>,
    // TODO: track addresses we've already seen and do not upgrade them
    // We need this because new addr might arrive while upgrade is in process for the same address.
    // seen_addrs: HashSet<SocketAddr>,
}

impl WebRTCListenStream {
    /// Constructs a `WebRTCListenStream` for incoming connections around
    /// the given `TcpListener`.
    fn new(
        listen_addr: SocketAddr,
        config: RTCConfiguration,
        udp_mux: Arc<dyn UDPMux + Send + Sync>,
        new_addr_rx: Arc<Mutex<mpsc::Receiver<SocketAddr>>>,
    ) -> Self {
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
            }
        };

        WebRTCListenStream {
            listen_addr,
            in_addr,
            pause: None,
            sleep_on_error: Duration::from_millis(100),
            config,
            udp_mux,
            new_addr_rx,
        }
    }
}

impl Stream for WebRTCListenStream {
    type Item =
        Result<ListenerEvent<BoxFuture<'static, Result<Connection<'static>, Error>>, Error>, Error>;

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
                InAddr::One { out } => {
                    if let Some(multiaddr) = out.take() {
                        return Poll::Ready(Some(Ok(ListenerEvent::NewAddress(multiaddr))));
                    }
                },
            }

            if let Some(mut pause) = me.pause.take() {
                match Pin::new(&mut pause).poll(cx) {
                    Poll::Ready(_) => {},
                    Poll::Pending => {
                        me.pause = Some(pause);
                        return Poll::Pending;
                    },
                }
            }

            return match Pin::new(&mut *me.new_addr_rx.lock().unwrap()).poll_next(cx) {
                Poll::Ready(Some(socket_addr)) => Poll::Ready(Some(Ok(ListenerEvent::Upgrade {
                    local_addr: ip_to_multiaddr(me.listen_addr.ip(), me.listen_addr.port()),
                    remote_addr: ip_to_multiaddr(socket_addr.ip(), socket_addr.port()),
                    upgrade: Box::pin(WebRTCUpgrade::new(
                        me.udp_mux.clone(),
                        me.config.clone(),
                        socket_addr,
                    )) as BoxFuture<'static, _>,
                }))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

impl WebRTCDirectTransport {
    async fn do_dial(self, addr: Multiaddr) -> Result<Connection<'static>, Error> {
        let mut inner_addr = addr.clone();

        let socket_addr = multiaddr_to_socketaddr(&inner_addr)
            .ok_or_else(|| Error::InvalidMultiaddr(addr.clone()))?;
        if socket_addr.port() == 0 || socket_addr.ip().is_unspecified() {
            return Err(Error::InvalidMultiaddr(addr.clone()));
        }

        let fingerprint = match inner_addr.pop() {
            Some(Protocol::XWebRTC(f)) => f,
            _ => {
                debug!("{} is not a WebRTC multiaddr", addr);
                return Err(Error::InvalidMultiaddr(addr));
            },
        };

        let config = self.config.clone();

        let remote = addr.clone(); // used for logging

        trace!("dialing address: {:?}", remote);

        let mut se = SettingEngine::default();
        {
            // Set both ICE user and password to fingerprint.
            // It will be checked by remote side when exchanging ICE messages.
            let f = self.cert_fingerprint();
            se.set_ice_credentials(f.clone(), f);
            se.set_udp_network(UDPNetwork::Muxed(self.udp_mux.clone()));
            // Act as a DTLS client (one which initiates a connection).
            se.set_answering_dtls_role(DTLSRole::Client)
                .map_err(Error::WebRTC)?;
        }

        let api = APIBuilder::new().with_setting_engine(se).build();

        let peer_connection = api
            .new_peer_connection(config)
            .map_err(Error::WebRTC)
            .await?;

        // Create a datachannel with label 'data'
        let data_channel = peer_connection.create_data_channel("data", None).await?;

        peer_connection
            .on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
                debug!("Peer Connection State has changed: {}", s);

                Box::pin(async {})
            }))
            .await;

        let (data_channel_rx, data_channel_tx) = oneshot::channel::<Arc<DetachedDataChannel>>();

        // Register channel opening handling
        let d = Arc::clone(&data_channel);
        data_channel
            .on_open(Box::new(move || {
                debug!("Data channel '{}'-'{}' open.", d.label(), d.id());

                let d2 = Arc::clone(&d);
                Box::pin(async move {
                    match d2.detach().await {
                        // TODO: remove unwrap
                        Ok(detached) => data_channel_rx.send(detached).unwrap(),
                        Err(e) => {
                            error!("Can't detach data channel: {}", e);
                        },
                    };
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

        let server_session_description = {
            let mut tt = TinyTemplate::new();
            tt.add_template("description", sdp::SERVER_SESSION_DESCRIPTION)
                .unwrap();

            let f = fingerprint_to_hex_string(&fingerprint);
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
                ufrag: f.clone(),
                pwd: f,
            };
            tt.render("description", &context).unwrap()
        };
        let sdp = RTCSessionDescription::answer(server_session_description.clone()).unwrap();
        // Set the local description and start UDP listeners
        // Note: this will start the gathering of ICE candidates
        peer_connection
            .set_remote_description(sdp)
            .map_err(Error::WebRTC)
            .await?;
        let mut answer = peer_connection
            .create_answer(None)
            .map_err(Error::WebRTC)
            .await?;
        answer.sdp = server_session_description;
        debug!("REMOTE ANSWER: {:?}", answer);

        // wait until data channel is opened and ready to use
        let data_channel = data_channel_tx
            .map_err(|e| Error::InternalError(e.to_string()))
            .await?;

        Ok(Connection::new(peer_connection, data_channel))
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
        _ => None,
    }
}

/// Turns an IP address and port into the corresponding WebRTC multiaddr.
fn socketaddr_to_multiaddr<'a>(
    socket_addr: &SocketAddr,
    fingerprint: Cow<'a, [u8; 32]>,
) -> Multiaddr {
    Multiaddr::empty()
        .with(socket_addr.ip().into())
        .with(Protocol::Udp(socket_addr.port()))
        .with(Protocol::XWebRTC(fingerprint))
}

fn fingerprint_to_hex_string(f: &Cow<'_, [u8; 32]>) -> String {
    hex::encode(f.as_ref()).to_uppercase()
}

// Tests //////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;
    use libp2p_core::{multiaddr::Protocol, Multiaddr, PeerId, Transport};
    use rcgen::KeyPair;
    use std::net::IpAddr;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tokio_crate as tokio;

    #[test]
    fn multiaddr_to_socketaddr_conversion() {
        assert!(
            multiaddr_to_socketaddr(&"/ip4/127.0.0.1/udp/1234".parse::<Multiaddr>().unwrap())
                .is_none()
        );

        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip4/127.0.0.1/udp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some( SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                    12345,
            ) )
        );

        assert!(
            multiaddr_to_socketaddr(
                &"/ip4/127.0.0.1/tcp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ).is_none()
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
            Some(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255)),
                    8080,
            ))
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/::1/udp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some( SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)),
                    12345,
            ) )
        );
        assert_eq!(
            multiaddr_to_socketaddr(
                &"/ip6/ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff/udp/8080/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                    .parse::<Multiaddr>()
                    .unwrap()
            ),
            Some( SocketAddr::new(
                    IpAddr::V6(Ipv6Addr::new(
                            65535, 65535, 65535, 65535, 65535, 65535, 65535, 65535,
                    )),
                    8080,
            ) )
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
            "/ip4/127.0.0.1/udp/12345/x-webrtc/ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B"
                .parse::<Multiaddr>()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn dialer_connects_to_listener_ipv4() {
        let _ = env_logger::builder().is_test(true).try_init();
        let a = "127.0.0.1:0".parse().unwrap();
        connect(a).await
    }

    #[tokio::test]
    async fn dialer_connects_to_listener_ipv6() {
        let _ = env_logger::builder().is_test(true).try_init();
        let a = "[::1]:0".parse().unwrap();
        connect(a).await;
    }

    async fn connect(listen_addr: SocketAddr) {
        let kp = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256).expect("key pair");
        let cert = RTCCertificate::from_key_pair(kp).expect("certificate");
        let transport = WebRTCDirectTransport::new(cert, listen_addr)
            .await
            .expect("transport");

        let mut listener = transport
            .clone()
            .listen_on(ip_to_multiaddr(listen_addr.ip(), listen_addr.port()))
            .expect("listener");

        let addr = listener
            .try_next()
            .await
            .expect("some event")
            .expect("no error")
            .into_new_address()
            .expect("listen address");

        assert_ne!(Some(Protocol::Udp(0)), addr.iter().nth(1));

        let inbound = async move {
            let (conn, _addr) = listener
                .try_filter_map(|e| future::ready(Ok(e.into_upgrade())))
                .try_next()
                .await
                .unwrap()
                .unwrap();
            conn.await
        };

        let outbound = transport
            .dial(addr.with(Protocol::XWebRTC(hex_to_cow(
                "ACD1E533EC271FCDE0275947F4D62A2B2331FF10C9DDE0298EB7B399B4BFF60B",
            ))))
            .unwrap();

        let (a, b) = futures::join!(inbound, outbound);
        a.and(b).unwrap();
    }
}
