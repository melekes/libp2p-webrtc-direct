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

use log::{debug, error};
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc_ice::udp_mux::UDPMux;

use std::net::SocketAddr;
use std::sync::Arc;

use crate::error::Error;
use crate::sdp;
use crate::transport::build_settings;
use crate::transport::Connection;

pub struct WebRTCUpgrade {}

impl WebRTCUpgrade {
    pub async fn new(
        udp_mux: Arc<dyn UDPMux + Send + Sync>,
        config: RTCConfiguration,
        socket_addr: SocketAddr,
    ) -> Result<Connection, Error> {
        let mut se = build_settings(udp_mux);
        // Act as a DTLS server (one which waits for a connection).
        se.set_answering_dtls_role(DTLSRole::Server)?;

        let api = APIBuilder::new().with_setting_engine(se).build();
        let peer_connection = api.new_peer_connection(config).await?;

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

        // Set the remote description to the predefined SDP
        let mut offer = peer_connection.create_offer(None).await?;
        offer.sdp = sdp::CLIENT_SESSION_DESCRIPTION.to_string();
        debug!("REMOTE OFFER: {:?}", offer);
        peer_connection.set_remote_description(offer).await?;

        let answer = peer_connection.create_answer(None).await?;
        // Set the local description and start UDP listeners
        // Note: this will start the gathering of ICE candidates
        debug!("LOCAL ANSWER: {:?}", answer);
        peer_connection.set_local_description(answer).await?;

        Ok(Connection {
            connection: peer_connection,
        })
    }
}
