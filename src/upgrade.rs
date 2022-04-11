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
use futures::TryFutureExt;
use log::{debug, error};
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc_data::data_channel::DataChannel as DetachedDataChannel;
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

        // Create a datachannel with label 'data'
        let data_channel = peer_connection.create_data_channel("data", None).await?;

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

        let (data_channel_rx, data_channel_tx) = oneshot::channel::<Arc<DetachedDataChannel>>();

        // Register channel opening handling
        let d = Arc::clone(&data_channel);
        data_channel
            .on_open(Box::new(move || {
                println!("Data channel '{}'-'{}' open.", d.label(), d.id());

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

        // Set the remote description to the predefined SDP
        let mut offer = peer_connection.create_offer(None).await?;
        offer.sdp = sdp::CLIENT_SESSION_DESCRIPTION.to_string();
        // TODO: - set IN IP4 {REMOTE_IP}
        //       - set ufrag and pwd to fingerprint
        debug!("REMOTE OFFER: {:?}", offer);
        peer_connection.set_remote_description(offer).await?;

        let answer = peer_connection.create_answer(None).await?;
        // Set the local description and start UDP listeners
        // Note: this will start the gathering of ICE candidates
        debug!("LOCAL ANSWER: {:?}", answer);
        peer_connection.set_local_description(answer).await?;

        // wait until data channel is opened and ready to use
        let data_channel = data_channel_tx
            .map_err(|e| Error::InternalError(e.to_string()))
            .await?;

        Ok(Connection {
            connection: peer_connection,
            data_channel,
        })
    }
}
