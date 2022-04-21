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

use futures::channel::oneshot;
use libp2p_core::multiaddr::{Multiaddr, Protocol};
use log::{debug, error, trace};
use std::sync::Arc;
use std::time::Duration;
use tinytemplate::TinyTemplate;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc_data::data_channel::DataChannel as DetachedDataChannel;
use webrtc_ice::udp_mux::UDPMux;
use webrtc_ice::udp_network::UDPNetwork;

use crate::connection::Connection;
use crate::error::Error;
use crate::sdp;
use crate::transport;

pub struct WebRTCUpgrade {}

impl WebRTCUpgrade {
    pub async fn new(
        udp_mux: Arc<dyn UDPMux + Send + Sync>,
        config: RTCConfiguration,
        addr: Multiaddr,
    ) -> Result<Connection<'static>, Error> {
        trace!("upgrading {}", addr);

        let mut se = SettingEngine::default();
        // Disable remote's fingerprint verification.
        // se.disable_certificate_fingerprint_verification(true);
        // Act as a lite ICE (ICE which does not send additional candidates).
        se.set_lite(true);
        // Set both ICE user and password to fingerprint.
        // It will be checked by remote side when exchanging ICE messages.
        let f = fingerprint_of_first_certificate(&config);
        se.set_ice_credentials(f.clone().replace(":", ""), f.replace(":", ""));
        se.set_udp_network(UDPNetwork::Muxed(udp_mux));

        let api = APIBuilder::new().with_setting_engine(se).build();
        let peer_connection = api.new_peer_connection(config).await?;

        // Create a datachannel with label 'data'
        let data_channel = peer_connection
            .create_data_channel(
                "data",
                Some(RTCDataChannelInit {
                    negotiated: Some(true),
                    id: Some(0),
                    ordered: None,
                    max_retransmits: None,
                    max_packet_life_time: None,
                    protocol: None,
                }),
            )
            .await?;

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

        let socket_addr = transport::multiaddr_to_socketaddr(&addr)
            .ok_or_else(|| Error::InvalidMultiaddr(addr.clone()))?;

        // Set the remote description to the predefined SDP
        let client_session_description = {
            let mut tt = TinyTemplate::new();
            tt.add_template("description", sdp::CLIENT_SESSION_DESCRIPTION)
                .unwrap();

            let fingerprint = match addr.iter().last() {
                Some(Protocol::XWebRTC(f)) => f,
                _ => {
                    debug!("{} is not a WebRTC multiaddr", addr);
                    return Err(Error::InvalidMultiaddr(addr));
                },
            };
            let f = transport::format_fingerprint(&fingerprint);
            let context = sdp::DescriptionContext {
                ip_version: if socket_addr.is_ipv4() {
                    sdp::IpVersion::IP4
                } else {
                    sdp::IpVersion::IP6
                },
                target_ip: socket_addr.ip(),
                target_port: socket_addr.port(),
                fingerprint: f.clone(),
                ufrag: f.clone().replace(":", ""),
                pwd: f.replace(":", ""),
            };
            tt.render("description", &context).unwrap()
        };
        let sdp = RTCSessionDescription::offer(client_session_description.clone()).unwrap();
        debug!("OFFER: {:?}", sdp);
        peer_connection.set_remote_description(sdp).await?;

        let answer = peer_connection.create_answer(None).await?;
        // Set the local description and start UDP listeners
        // Note: this will start the gathering of ICE candidates
        debug!("ANSWER: {:?}", answer);
        peer_connection.set_local_description(answer).await?;

        // wait until data channel is opened and ready to use
        match tokio_crate::time::timeout(Duration::from_secs(10), data_channel_tx).await {
            Ok(Ok(dc)) => Ok(Connection::new(peer_connection, dc)),
            Err(_) | Ok(Err(_)) => Err(Error::InternalError(
                "data channel opening took longer than 10 seconds".into(),
            )),
        }
    }
}

fn fingerprint_of_first_certificate(config: &RTCConfiguration) -> String {
    let fingerprints = config
        .certificates
        .first()
        .expect("at least one certificate")
        .get_fingerprints()
        .expect("fingerprints to succeed");
    fingerprints.first().unwrap().value.to_owned()
}
