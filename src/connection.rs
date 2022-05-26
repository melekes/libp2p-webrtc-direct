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
use futures::channel::mpsc::{self, Receiver, Sender};
use futures::lock::Mutex as FutMutex;
use futures::{channel::oneshot, future::BoxFuture, prelude::*, ready};
use futures_lite::stream::StreamExt;
use libp2p_core::muxing::{StreamMuxer, StreamMuxerEvent};
use log::{debug, error, trace};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc_data::data_channel::DataChannel as DetachedDataChannel;

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

pub(crate) use poll_data_channel::PollDataChannel;

/// A WebRTC connection, wrapping [`RTCPeerConnection`] and implementing [`StreamMuxer`] trait.
pub struct Connection {
    connection_inner: Arc<FutMutex<ConnectionInner>>, // uses futures mutex because used in async code (see open_outbound)
    data_channels_inner: StdMutex<DataChannelsInner>,
}

struct ConnectionInner {
    /// `RTCPeerConnection` to the remote peer.
    rtc_conn: RTCPeerConnection,
    /// The next outbound data channel ID.
    next_outbound_channel_id: u16,
}

struct DataChannelsInner {
    /// A map of data channels.
    map: FnvHashMap<u16, PollDataChannel>,
    /// Channel onto which incoming data channels are put.
    incoming_data_channels_rx: Receiver<Arc<DetachedDataChannel>>,
}

impl Connection {
    /// Creates a new connection.
    pub async fn new(rtc_conn: RTCPeerConnection) -> Self {
        let (data_channel_tx, data_channel_rx) = mpsc::channel(10);

        Connection::register_incoming_data_channels_handler(&rtc_conn, data_channel_tx).await;

        Self {
            connection_inner: Arc::new(FutMutex::new(ConnectionInner {
                rtc_conn,
                // Starts with `2` because `1` is reserved for the data channel used for authentication.
                next_outbound_channel_id: 2,
            })),
            data_channels_inner: StdMutex::new(DataChannelsInner {
                map: FnvHashMap::default(),
                incoming_data_channels_rx: data_channel_rx,
            }),
        }
    }

    /// Registers a handler for incoming data channels.
    async fn register_incoming_data_channels_handler(
        rtc_conn: &RTCPeerConnection,
        data_channel_tx: Sender<Arc<DetachedDataChannel>>,
    ) {
        rtc_conn
            .on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
                debug!(
                    "Incoming data channel '{}'-'{}'",
                    data_channel.label(),
                    data_channel.id()
                );
                let data_channel = data_channel.clone();

                let mut data_channel_tx = data_channel_tx.clone();
                Box::pin(async move {
                    data_channel
                        .on_open({
                            let data_channel = data_channel.clone();
                            Box::new(move || {
                                debug!(
                                    "Data channel '{}'-'{}' open",
                                    data_channel.label(),
                                    data_channel.id()
                                );

                                Box::pin(async move {
                                    let data_channel = data_channel.clone();
                                    match data_channel.detach().await {
                                        Ok(detached) => {
                                            if let Err(e) = data_channel_tx.try_send(detached) {
                                                // This can happen if the client is not reading
                                                // events (using `poll_event`) fast enough, which
                                                // generally shouldn't be the case.
                                                error!("Can't send data channel: {}", e);
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
                })
            }))
            .await;
    }
}

impl<'a> StreamMuxer for Connection {
    type Substream = PollDataChannel;
    type OutboundSubstream = BoxFuture<'static, Result<Arc<DetachedDataChannel>, Self::Error>>;
    type Error = io::Error;

    fn poll_event(
        &self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<StreamMuxerEvent<Self::Substream>, Self::Error>> {
        let mut data_channels_inner = self.data_channels_inner.lock().unwrap();
        match ready!(data_channels_inner.incoming_data_channels_rx.poll_next(cx)) {
            Some(detached) => {
                let ch = PollDataChannel::new(detached);

                data_channels_inner
                    .map
                    .insert(ch.stream_identifier(), ch.clone());

                Poll::Ready(Ok(StreamMuxerEvent::InboundSubstream(ch)))
            },
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Other,
                "incoming_data_channels_rx is closed (no messages left)",
            ))),
        }
    }

    fn open_outbound(&self) -> Self::OutboundSubstream {
        let connection_inner = self.connection_inner.clone();

        Box::pin(async move {
            let mut connection_inner = connection_inner.lock().await;

            let channel_id = connection_inner.next_outbound_channel_id;
            connection_inner.next_outbound_channel_id += 1;

            trace!("Opening outbound substream {}", channel_id);

            // Create a datachannel with label 'data'
            let data_channel = connection_inner
                .rtc_conn
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
                .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("webrtc error: {}", e)))
                .await?;

            // No need to hold the lock during the DTLS handshake.
            drop(connection_inner);

            let (data_channel_rx, data_channel_tx) = oneshot::channel::<Arc<DetachedDataChannel>>();

            // Wait until the data channel is opened and detach it.
            data_channel
                .on_open({
                    let data_channel = data_channel.clone();
                    Box::new(move || {
                        debug!(
                            "Data channel '{}'-'{}' open",
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
                Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
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
                let ch = PollDataChannel::new(detached);

                let mut data_channels_inner = self.data_channels_inner.lock().unwrap();
                data_channels_inner
                    .map
                    .insert(ch.stream_identifier(), ch.clone());

                Poll::Ready(Ok(ch))
            },
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// NOTE: `_s` might be waiting at one of the await points, and dropping the future will
    /// abruptly interrupt the execution.
    fn destroy_outbound(&self, _s: Self::OutboundSubstream) {}

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
        let mut data_channels_inner = self.data_channels_inner.lock().unwrap();
        data_channels_inner.map.remove(&s.stream_identifier());
    }

    fn close(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // First, flush all the buffered data.
        match ready!(self.flush_all(cx)) {
            Ok(_) => {
                // Second, shutdown all the substreams.
                let mut data_channels_inner = self.data_channels_inner.lock().unwrap();
                for (_, ch) in data_channels_inner.map.iter_mut() {
                    match ready!(self.shutdown_substream(cx, ch)) {
                        Ok(_) => continue,
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                }

                // Third, close `incoming_data_channels_rx`
                data_channels_inner.incoming_data_channels_rx.close();

                Poll::Ready(Ok(()))
            },
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn flush_all(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut data_channels_inner = self.data_channels_inner.lock().unwrap();
        for (_, ch) in data_channels_inner.map.iter_mut() {
            match ready!(self.flush_substream(cx, ch)) {
                Ok(_) => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(Ok(()))
    }
}
