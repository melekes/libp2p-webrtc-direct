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

use libp2p_core::{
    connection::Endpoint,
    multiaddr::{Multiaddr, Protocol},
    transport::{ListenerEvent, TransportError},
    Transport,
};

use async_std::net::IpAddr;
use async_std::net::SocketAddr;
use async_std::sync::Arc;
use bytes::Bytes;
use futures::{future::BoxFuture, prelude::*, stream::BoxStream};
use log::{debug, error, trace};
use serde::Serialize;
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
use webrtc_ice::udp_network::EphemeralUDP;
use webrtc_ice::udp_network::UDPNetwork;

use std::marker::PhantomData;

use crate::error::Error;

/// An SDP message that constitutes the offer.
/// Main RFC: <https://datatracker.ietf.org/doc/html/rfc8866>
/// `sctp-port` and `max-message-size` attrs RFC: <https://datatracker.ietf.org/doc/html/rfc8841>
/// `group` and `mid` attrs RFC: <https://datatracker.ietf.org/doc/html/rfc9143>
/// `ice-ufrag`, `ice-pwd` and `ice-options` attrs RFC: <https://datatracker.ietf.org/doc/html/rfc8839>
/// `setup` attr RFC: <https://datatracker.ietf.org/doc/html/rfc8122>
///
/// Short description:
///     v=<protocol-version>
///     o=<username> <sess-id> <sess-version> <nettype> <addrtype> <unicast-address>
///     s=<session name>
///     c=<nettype> <addrtype> <connection-address>
///     t=<start-time> <stop-time>
///
///     m=<media> <port> <proto> <fmt> ...
///     a=mid:<MID>
///     a=ice-options:ice2
///     a=ice-ufrag:<ICE user>
///     a=ice-pwd:<ICE password>
///     a=setup:<setup>
///     a=sctp-port:<value>
///     a=max-message-size:<value>
const CLIENT_SESSION_DESCRIPTION: &'static str = "v=0
o=- 0 0 IN IP4 0.0.0.0
s=-
c=IN IP4 0.0.0.0
t=0 0

m=application 9 UDP/DTLS/SCTP webrtc-datachannel
a=mid:0
a=ice-options:ice2
a=ice-ufrag:user
a=ice-pwd:password
a=fingerprint:sha-256 invalidFingerprint
a=setup:actpass
a=sctp-port:5000
a=max-message-size:100000
";

// Version of the SDP protocol. Always 0. (RFC8866)
//
// Identifies the creator of the SDP document. We are allowed to use dummy values
// (`-` and `0.0.0.0`) to remain anonymous, which we do. Note that "IN" means
// "Internet". (RFC8866)
//
// Name for the session. We are allowed to pass a dummy `-`. (RFC8866)
//
// Start and end of the validity of the session. `0 0` means that the session never
// expires. (RFC8866)
//
// A lite implementation is only appropriate for devices that will
// *always* be connected to the public Internet and have a public
// IP address at which it can receive packets from any
// correspondent.  ICE will not function when a lite implementation
// is placed behind a NAT (RFC8445).
//
// A `m=` line describes a request to establish a certain protocol.
// The protocol in this line (i.e. `TCP/DTLS/SCTP` or `UDP/DTLS/SCTP`) must always be
// the same as the one in the offer. We know that this is true because we tweak the
// offer to match the protocol.
// The `<fmt>` component must always be `pc-datachannel` for WebRTC.
// The rest of the SDP payload adds attributes to this specific media stream.
// RFCs: 8839, 8866, 8841
//
// Indicates the IP address of the remote.
// Note that "IN" means "Internet".
//
// Media ID - uniquely identifies this media stream (RFC9143).
//
// Indicates that we are complying with RFC8839 (as oppposed to the legacy RFC5245).
//
// ICE username and password, which are used for establishing and
// maintaining the ICE connection. (RFC8839)
// MUST match ones used by the answerer (server).
//
// Fingerprint of the certificate that the server will use during the TLS
// handshake. (RFC8122)
// As explained at the top-level documentation, we use a hardcoded certificate.
// MUST be derived from the certificate used by the answerer (server).
// TODO: proper certificate and fingerprint
//
// "TLS ID" uniquely identifies a TLS association.
// The ICE protocol uses a "TLS ID" system to indicate whether a fresh DTLS connection
// must be reopened in case of ICE renegotiation. Considering that ICE renegotiations
// never happen in our use case, we can simply put a random value and not care about
// it. Note however that the TLS ID in the answer must be present if and only if the
// offer contains one. (RFC8842)
// TODO: is it true that renegotiations never happen? what about a connection closing?
// TODO: right now browsers don't send it "a=tls-id:" + genRandomPayload(120) + "\n" +
// "tls-id" attribute MUST be present in the initial offer and respective answer (RFC8839).
//
// Indicates that the remote DTLS server will only listen for incoming
// connections. (RFC5763)
// The answerer (server) MUST not be located behind a NAT (RFC6135).
//
// The SCTP port (RFC8841)
// Note it's different from the "m=" line port value, which
// indicates the port of the underlying transport-layer protocol
// (UDP or TCP)
//
// The maximum SCTP user message size (in bytes) (RFC8841)
//
// A transport address for a candidate that can be used for connectivity checks (RFC8839).
const SERVER_SESSION_DESCRIPTION: &'static str = "v=0
o=- 0 0 IN IP {IP_VERSION} {TARGET_IP}
s=-
t=0 0
a=ice-lite
m=application {TARGET_PORT} {PROTOCOL}/DTLS/SCTP webrtc-datachannel
c=IN IP {IP_VERSION} {TARGET_IP}
a=mid:0
a=ice-options:ice2
a=ice-ufrag:user
a=ice-pwd:password
a=fingerprint:sha-256 {FINGERPRINT}

a=setup:passive
a=sctp-port:5000
a=max-message-size:100000
a=candidate:1 1 {PROTOCOL} 2113667327 {TARGET_IP} {TARGET_PORT} typ host
";

#[derive(Serialize)]
enum IpVersion {
    IP4,
    IP6,
}

#[derive(Serialize)]
enum TransportProtocol {
    TCP,
    UDP,
}

#[derive(Serialize)]
struct Context {
    ip_version: IpVersion,
    target_ip: IpAddr,
    target_port: u16,
    protocol: TransportProtocol,
    fingerprint: String,
}

// A WebRTC connection.
pub struct Connection<T> {
    connection: RTCPeerConnection,
    // receiver: BoxStream<'static, Result<Incoming, connection::Error>>,
    // sender: Pin<Box<dyn Sink<Outgoing, Error = connection::Error> + Send>>,
    _marker: std::marker::PhantomData<T>,
}

/// A WebRTC transport based on either TCP or UDP transport.
pub struct WebRTCDirectTransport<T> {
    pub config: RTCConfiguration,

    transport: T,
}

impl<T> WebRTCDirectTransport<T> {
    /// Create a new transport based on the inner transport.
    ///
    /// See [`libp2p-tcp`](https://docs.rs/libp2p-tcp/) for constructing the inner transport.
    pub fn new(transport: T, certificate: RTCCertificate) -> Self {
        let config = RTCConfiguration {
            certificates: vec![certificate],
            ..Default::default()
        };

        Self { transport, config }
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
            Some(p @ Protocol::XWebRTC(_)) => p,
            _ => {
                debug!("{} is not a WebRTC multiaddr", addr);
                return Err(TransportError::MultiaddrNotSupported(addr));
            },
        };

        let (socket_addr, _) = multiaddr_to_socketaddr(&inner_addr)
            .ok_or_else(|| TransportError::MultiaddrNotSupported(addr))?;

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
                },
                ListenerEvent::AddressExpired(mut a) => {
                    a = a.with(proto.clone());
                    ListenerEvent::AddressExpired(a)
                },
                ListenerEvent::Error(err) => ListenerEvent::Error(Error::Transport(err)),
                ListenerEvent::Upgrade {
                    upgrade,
                    mut local_addr,
                    mut remote_addr,
                } => {
                    local_addr = local_addr.with(proto.clone());
                    remote_addr = remote_addr.with(proto.clone());
                    let remote = remote_addr.clone(); // used for logging

                    let upgrade = async move {
                        let _stream = upgrade.map_err(Error::Transport).await?;
                        trace!("Incoming connection from {}", remote);

                        let mut se = SettingEngine::default();

                        // Disable remote's fingerprint verification.
                        se.disable_certificate_fingerprint_verification(true);

                        // Act as a lite ICE (ICE which does not send additional candidates).
                        se.set_lite(true);

                        // Set both ICE user and password to fingerprint.
                        // It will be checked by remote side when exchanging ICE messages.
                        se.set_ice_credentials("user".to_string(), "password".to_string());

                        // Act as a DTLS server (wait for ClientHello message from the remote).
                        se.set_answering_dtls_role(DTLSRole::Server)?;

                        // UDP network ([`UDPNetwork`]) is used for ICE traffic.
                        // Only one UDP port is needed because there's going to be just one
                        // candidate. Hence `port_min` == `port_max`.
                        //
                        // In case of TCP transport, there will be two open ports (same number; one
                        // for TCP and one for UDP).
                        se.set_udp_network(UDPNetwork::Ephemeral(
                            EphemeralUDP::new(socket_addr.port(), socket_addr.port())
                                .map_err(|e| Error::WebRTC(webrtc::Error::Ice(e)))?,
                        ));

                        let api = APIBuilder::new().with_setting_engine(se).build();

                        let peer_connection = api.new_peer_connection(self.config).await?;

                        peer_connection
                            .on_peer_connection_state_change(Box::new(
                                move |s: RTCPeerConnectionState| {
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
                                },
                            ))
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
                                        debug!(
                                            "Message from DataChannel '{}': '{}'",
                                            d_label, msg_str
                                        );
                                        Box::pin(async {})
                                    }))
                                    .await;
                                })
                            }))
                            .await;

                        // Set the remote description to the predefined SDP
                        let mut offer = peer_connection.create_offer(None).await?;
                        offer.sdp = CLIENT_SESSION_DESCRIPTION.to_string();
                        debug!("REMOTE OFFER: {:?}", offer);
                        peer_connection.set_remote_description(offer).await?;

                        let answer = peer_connection.create_answer(None).await?;
                        // Set the local description and start UDP listeners
                        // Note: this will start the gathering of ICE candidates
                        debug!("LOCAL ANSWER: {:?}", answer);
                        peer_connection.set_local_description(answer).await?;

                        Ok(Connection {
                            connection: peer_connection,
                            _marker: PhantomData,
                        })
                    };

                    ListenerEvent::Upgrade {
                        upgrade: Box::pin(upgrade) as BoxFuture<'static, _>,
                        local_addr,
                        remote_addr,
                    }
                },
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
        let mut inner_addr = addr.clone();

        let fingerprint = match inner_addr.pop() {
            Some(Protocol::XWebRTC(f)) => f,
            _ => {
                debug!("{} is not a WebRTC multiaddr", addr);
                return Err(TransportError::MultiaddrNotSupported(addr));
            },
        };

        let (socket_addr, transport_protocol) = multiaddr_to_socketaddr(&inner_addr)
            .ok_or_else(|| TransportError::MultiaddrNotSupported(inner_addr))?;

        let mut tt = TinyTemplate::new();
        tt.add_template("description", SERVER_SESSION_DESCRIPTION)
            .unwrap();

        let context = Context {
            ip_version: {
                if socket_addr.is_ipv4() {
                    IpVersion::IP4
                } else {
                    IpVersion::IP6
                }
            },
            target_ip: socket_addr.ip(),
            target_port: socket_addr.port(),
            protocol: transport_protocol,
            fingerprint: hex::encode(fingerprint.as_ref()),
        };
        let server_session_description = tt.render("description", &context).unwrap();

        let future = async move {
            let remote = addr.clone(); // used for logging

            trace!("dialing address: {:?}", remote);

            let dial = match role_override {
                Endpoint::Dialer => self.transport.dial(addr),
                Endpoint::Listener => self.transport.dial_as_listener(addr),
            }
            .map_err(|e| match e {
                TransportError::MultiaddrNotSupported(a) => Error::InvalidMultiaddr(a),
                TransportError::Other(e) => Error::Transport(e),
            })?;

            let _stream = dial.map_err(Error::Transport).await?;
            trace!("transport connection to {} established", remote);

            // TODO: dedup!
            let mut se = SettingEngine::default();

            // Disable remote's fingerprint verification.
            se.disable_certificate_fingerprint_verification(true);

            // Act as a lite ICE (ICE which does not send additional candidates).
            se.set_lite(true);

            // Set both ICE user and password to fingerprint.
            // It will be checked by remote side when exchanging ICE messages.
            se.set_ice_credentials("user".to_string(), "password".to_string());

            // Act as a DTLS server (wait for ClientHello message from the remote).
            se.set_answering_dtls_role(DTLSRole::Client)
                .map_err(Error::WebRTC)?;

            // UDP network ([`UDPNetwork`]) is used for ICE traffic.
            // Only one UDP port is needed because there's going to be just one
            // candidate. Hence `port_min` == `port_max`.
            //
            // In case of TCP transport, there will be two open ports (same number; one
            // for TCP and one for UDP).
            se.set_udp_network(UDPNetwork::Ephemeral(
                EphemeralUDP::new(socket_addr.port(), socket_addr.port())
                    .map_err(|e| Error::WebRTC(webrtc::Error::Ice(e)))?,
            ));

            let api = APIBuilder::new().with_setting_engine(se).build();

            let peer_connection = api
                .new_peer_connection(self.config)
                .map_err(Error::WebRTC)
                .await?;

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
                _marker: PhantomData,
            })
        };

        Ok(Box::pin(future))
    }
}

/// Tries to turn a WebRTC multiaddress into a [`SocketAddr`]. Returns None if the format of the
/// multiaddr is wrong.
fn multiaddr_to_socketaddr(addr: &Multiaddr) -> Option<(SocketAddr, TransportProtocol)> {
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
            Some((SocketAddr::new(ip.into(), port), TransportProtocol::UDP))
        },
        (Protocol::Ip6(ip), Protocol::Udp(port), Protocol::XWebRTC(_)) => {
            Some((SocketAddr::new(ip.into(), port), TransportProtocol::UDP))
        },
        (Protocol::Ip4(ip), Protocol::Tcp(port), Protocol::XWebRTC(_)) => {
            Some((SocketAddr::new(ip.into(), port), TransportProtocol::TCP))
        },
        (Protocol::Ip6(ip), Protocol::Tcp(port), Protocol::XWebRTC(_)) => {
            Some((SocketAddr::new(ip.into(), port), TransportProtocol::TCP))
        },
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
