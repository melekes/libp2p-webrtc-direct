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

use serde::Serialize;

use std::net::IpAddr;

/// An SDP message that constitutes the offer.
///
/// Main RFC: <https://datatracker.ietf.org/doc/html/rfc8866>
/// `sctp-port` and `max-message-size` attrs RFC: <https://datatracker.ietf.org/doc/html/rfc8841>
/// `group` and `mid` attrs RFC: <https://datatracker.ietf.org/doc/html/rfc9143>
/// `ice-ufrag`, `ice-pwd` and `ice-options` attrs RFC: <https://datatracker.ietf.org/doc/html/rfc8839>
/// `setup` attr RFC: <https://datatracker.ietf.org/doc/html/rfc8122>
///
/// Short description:
///
/// v=<protocol-version>
/// o=<username> <sess-id> <sess-version> <nettype> <addrtype> <unicast-address>
/// s=<session name>
/// c=<nettype> <addrtype> <connection-address>
/// t=<start-time> <stop-time>
///
/// m=<media> <port> <proto> <fmt> ...
/// a=mid:<MID>
/// a=ice-options:ice2
/// a=ice-ufrag:<ICE user>
/// a=ice-pwd:<ICE password>
/// a=setup:<setup>
/// a=sctp-port:<value>
/// a=max-message-size:<value>
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
pub const SERVER_SESSION_DESCRIPTION: &'static str = "v=0
o=- 0 0 IN IP {IP_VERSION} {TARGET_IP}
s=-
t=0 0
a=ice-lite
m=application {TARGET_PORT} UDP/DTLS/SCTP webrtc-datachannel
c=IN IP {IP_VERSION} {TARGET_IP}
a=mid:0
a=ice-options:ice2
a=ice-ufrag:user
a=ice-pwd:password
a=fingerprint:sha-256 {FINGERPRINT}

a=setup:passive
a=sctp-port:5000
a=max-message-size:100000
a=candidate:1 1 UDP 2113667327 {TARGET_IP} {TARGET_PORT} typ host
";

/// Indicates the IP version used in WebRTC: `IP4` or `IP6`.
#[derive(Serialize)]
pub enum IpVersion {
    IP4,
    IP6,
}

/// Context passed to the templating engine, which replaces the above placeholders (e.g.
/// `{IP_VERSION}`) with real values.
#[derive(Serialize)]
pub struct SessionDescriptionContext {
    pub ip_version: IpVersion,
    pub target_ip: IpAddr,
    pub target_port: u16,
    pub fingerprint: String,
}
