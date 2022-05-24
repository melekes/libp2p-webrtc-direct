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

mod poll_data_channel;

use fnv::FnvHashMap;
use futures::lock::Mutex;
use futures::{channel::oneshot, prelude::*, ready};
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent};
use log::{debug, error, trace};
use thiserror::Error;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc_data::data_channel::DataChannel as DetachedDataChannel;

use std::io;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicU16, Ordering},
    Arc, RwLock,
};
use std::task::{Context, Poll};

use poll_data_channel::PollDataChannel;

pub type SubstreamId = u16;

/// Error in WebRTC.
#[derive(Error, Debug)]
pub enum ConnectionError {
    #[error("webrtc error: {0}")]
    WebRTC(#[from] webrtc::Error),
    #[error("internal error: {0} (see debug logs)")]
    Internal(String),
}

impl From<ConnectionError> for std::io::Error {
    fn from(err: ConnectionError) -> Self {
        std::io::Error::new(std::io::ErrorKind::Other, err)
    }
}

/// A WebRTC connection over a single data channel. See lib documentation for
/// the reasoning as to why a single data channel is being used.
pub struct Connection {
    /// `RTCPeerConnection` to the remote peer.
    connection: Mutex<RTCPeerConnection>,
    /// A map of data channels
    data_channels: std::sync::Mutex<FnvHashMap<SubstreamId, PollDataChannel>>,
    /// The next data channel ID
    next_channel_id: AtomicU16,
}

impl Connection {
    pub fn new(connection: RTCPeerConnection) -> Self {
        Self {
            connection: Mutex::new(connection),
            data_channels: std::sync::Mutex::new(FnvHashMap::default()),
            next_channel_id: AtomicU16::new(0),
        }
    }
}

impl StreamMuxer for Connection {
    type Substream = PollDataChannel;
    type OutboundSubstream =
        Pin<Box<dyn Future<Output = Result<Arc<DetachedDataChannel>, Self::Error>> + Send>>;
    type Error = io::Error;

    fn poll_event(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<StreamMuxerEvent<Self::Substream>, Self::Error>> {
        unimplemented!();
    }

    fn open_outbound(&self) -> Self::OutboundSubstream {
        Box::pin(async move {
            let mut connection = self.connection.lock().await;

            let channel_id = self.next_channel_id.fetch_add(1, Ordering::Relaxed);
            trace!("Opening outbound substream {}", channel_id);

            // Create a datachannel with label 'data'
            let data_channel = connection
                .create_data_channel(
                    "data",
                    Some(RTCDataChannelInit {
                        negotiated: None,
                        id: Some(channel_id),
                        ordered: None,
                        max_retransmits: None,
                        max_packet_life_time: None,
                        protocol: None,
                    }),
                )
                .map_err(|e| io::Error::from(ConnectionError::WebRTC(e)))
                .await?;

            // No need to hold the lock during the DTLS handshake.
            drop(connection);

            let (data_channel_rx, data_channel_tx) = oneshot::channel::<Arc<DetachedDataChannel>>();

            // Wait until the data channel is opened and detach it.
            data_channel
                .on_open({
                    let data_channel = data_channel.clone();
                    Box::new(move || {
                        debug!(
                            "Data channel '{}'-'{}' open.",
                            data_channel.label(),
                            data_channel.id()
                        );

                        Box::pin(async move {
                            let data_channel = data_channel.clone();
                            match data_channel.detach().await {
                                Ok(detached) => {
                                    if let Err(_) = data_channel_rx.send(detached) {
                                        error!("data_channel_tx dropped");
                                    }
                                },
                                Err(e) => {
                                    error!("Can't detach data channel: {}", e);
                                },
                            };
                        })
                    })
                })
                .await;

            // Wait until data channel is opened and ready to use
            match data_channel_tx.await {
                Ok(detached) => Ok(detached),
                Err(e) => Err(io::Error::from(ConnectionError::Internal(e.to_string()))),
            }
        })
    }

    fn poll_outbound(
        &self,
        cx: &mut Context<'_>,
        s: &mut Self::OutboundSubstream,
    ) -> Poll<Result<Self::Substream, Self::Error>> {
        match ready!(s.as_mut().poll(cx)) {
            Ok(detached) => {
                let dc = PollDataChannel::new(detached);

                let channels = self.data_channels.lock().unwrap();
                channels.insert(dc.stream_identifier(), dc.clone());

                Poll::Ready(Ok(dc))
            },
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn destroy_outbound(&self, _s: Self::OutboundSubstream) {
        // noop
    }

    fn read_substream(
        &self,
        cx: &mut Context<'_>,
        s: &mut Self::Substream,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Self::Error>> {
        Pin::new(s).poll_read(cx, buf)
    }

    fn write_substream(
        &self,
        cx: &mut Context<'_>,
        s: &mut Self::Substream,
        buf: &[u8],
    ) -> Poll<Result<usize, Self::Error>> {
        Pin::new(s).poll_write(cx, buf)
    }

    fn flush_substream(
        &self,
        cx: &mut Context<'_>,
        s: &mut Self::Substream,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(s).poll_flush(cx)
    }

    fn shutdown_substream(
        &self,
        cx: &mut Context<'_>,
        s: &mut Self::Substream,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(s).poll_close(cx)
    }

    fn destroy_substream(&self, s: Self::Substream) {
        let channels = self.data_channels.lock().unwrap();
        channels.remove(&s.stream_identifier());
    }

    fn close(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match ready!(self.flush_all(cx)) {
            Ok(_) => {
                for (_, dc) in *self.data_channels.lock().unwrap() {
                    match ready!(self.shutdown_substream(cx, &mut dc)) {
                        Ok(_) => continue,
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }
                Poll::Ready(Ok(()))
            },
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn flush_all(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        for (_, dc) in *self.data_channels.lock().unwrap() {
            match ready!(self.flush_substream(cx, &mut dc)) {
                Ok(_) => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(Ok(()))
    }
}
