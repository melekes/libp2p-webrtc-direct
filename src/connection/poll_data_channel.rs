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

use futures::prelude::*;
use webrtc_data::data_channel::DataChannel;
use webrtc_data::data_channel::PollDataChannel as RTCPollDataChannel;

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

#[derive(Debug)]
pub struct PollDataChannel(RTCPollDataChannel);

impl PollDataChannel {
    /// Constructs a new `PollDataChannel`.
    pub fn new(data_channel: Arc<DataChannel>) -> Self {
        Self(RTCPollDataChannel::new(data_channel))
    }

    /// Get back the inner data_channel.
    pub fn into_inner(self) -> RTCPollDataChannel {
        self.0
    }

    /// Obtain a clone of the inner data_channel.
    pub fn clone_inner(&self) -> RTCPollDataChannel {
        self.0.clone()
    }

    /// MessagesSent returns the number of messages sent
    pub fn messages_sent(&self) -> usize {
        self.0.messages_sent()
    }

    /// MessagesReceived returns the number of messages received
    pub fn messages_received(&self) -> usize {
        self.0.messages_received()
    }

    /// BytesSent returns the number of bytes sent
    pub fn bytes_sent(&self) -> usize {
        self.0.bytes_sent()
    }

    /// BytesReceived returns the number of bytes received
    pub fn bytes_received(&self) -> usize {
        self.0.bytes_received()
    }

    /// StreamIdentifier returns the Stream identifier associated to the stream.
    pub fn stream_identifier(&self) -> u16 {
        self.0.stream_identifier()
    }

    /// BufferedAmount returns the number of bytes of data currently queued to be
    /// sent over this stream.
    pub fn buffered_amount(&self) -> usize {
        self.0.buffered_amount()
    }

    /// BufferedAmountLowThreshold returns the number of bytes of buffered outgoing
    /// data that is considered "low." Defaults to 0.
    pub fn buffered_amount_low_threshold(&self) -> usize {
        self.0.buffered_amount_low_threshold()
    }

    // TODO
    // Set the capacity of the temporary read buffer (default: 8192).
    // pub fn set_read_buf_capacity(&mut self, capacity: usize) {
    //     self.0.set_read_buf_capacity(capacity)
    // }
}

impl AsyncRead for PollDataChannel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut read_buf = tokio_crate::io::ReadBuf::new(buf);
        futures::ready!(tokio_crate::io::AsyncRead::poll_read(
            Pin::new(&mut self.0),
            cx,
            &mut read_buf
        ))?;
        Poll::Ready(Ok(read_buf.filled().len()))
    }
}

impl AsyncWrite for PollDataChannel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        tokio_crate::io::AsyncWrite::poll_write(Pin::new(&mut self.0), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tokio_crate::io::AsyncWrite::poll_flush(Pin::new(&mut self.0), cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        tokio_crate::io::AsyncWrite::poll_shutdown(Pin::new(&mut self.0), cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        tokio_crate::io::AsyncWrite::poll_write_vectored(Pin::new(&mut self.0), cx, bufs)
    }
}

impl Clone for PollDataChannel {
    fn clone(&self) -> PollDataChannel {
        PollDataChannel(self.clone_inner())
    }
}

//use bytes::Bytes;

//use futures::prelude::*;
//use futures::ready;
//use webrtc_data::data_channel::DataChannel;
//use webrtc_data::Error;

//use std::fmt;
//use std::io;
//use std::pin::Pin;
//use std::sync::Arc;
//use std::task::{Context, Poll};

///// Default capacity of the temporary read buffer used by [`PollStream`].
//const DEFAULT_READ_BUF_SIZE: usize = 8192;

///// State of the read `Future` in [`PollStream`].
//enum ReadFut {
//    /// Nothing in progress.
//    Idle,
//    /// Reading data from the underlying stream.
//    Reading(Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>> + Send>>),
//    /// Finished reading, but there's unread data in the temporary buffer.
//    RemainingData(Vec<u8>),
//}

//impl ReadFut {
//    /// Gets a mutable reference to the future stored inside `Reading(future)`.
//    ///
//    /// # Panics
//    ///
//    /// Panics if `ReadFut` variant is not `Reading`.
//    fn get_reading_mut(
//        &mut self,
//    ) -> &mut Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>> + Send>> {
//        match self {
//            ReadFut::Reading(ref mut fut) => fut,
//            _ => panic!("expected ReadFut to be Reading"),
//        }
//    }
//}

///// A wrapper around around [`DataChannel`], which implements [`AsyncRead`] and
///// [`AsyncWrite`].
/////
///// Both `poll_read` and `poll_write` calls allocate temporary buffers, which results in an
///// additional overhead.
//pub struct PollDataChannel {
//    data_channel: Arc<DataChannel>,

//    read_fut: ReadFut,
//    write_fut: Option<Pin<Box<dyn Future<Output = Result<usize, Error>> + Send>>>,
//    shutdown_fut: Option<Pin<Box<dyn Future<Output = Result<(), Error>> + Send>>>,

//    read_buf_cap: usize,
//}

//impl PollDataChannel {
//    /// Constructs a new [`PollDataChannel`].
//    pub fn new(data_channel: Arc<DataChannel>) -> Self {
//        Self {
//            data_channel,
//            read_fut: ReadFut::Idle,
//            write_fut: None,
//            shutdown_fut: None,
//            read_buf_cap: DEFAULT_READ_BUF_SIZE,
//        }
//    }

//    /// Get back the inner data_channel.
//    pub fn into_inner(self) -> Arc<DataChannel> {
//        self.data_channel
//    }

//    /// Obtain a clone of the inner data_channel.
//    pub fn clone_inner(&self) -> Arc<DataChannel> {
//        self.data_channel.clone()
//    }

//    /// Set the capacity of the temporary read buffer (default: 8192).
//    pub fn set_read_buf_capacity(&mut self, cap: usize) {
//        self.read_buf_cap = cap
//    }

//    /// StreamIdentifier returns the Stream identifier associated to the stream.
//    pub fn stream_identifier(&self) -> u16 {
//        self.data_channel.stream_identifier()
//    }
//}

//impl AsyncRead for PollDataChannel {
//    fn poll_read(
//        mut self: Pin<&mut Self>,
//        cx: &mut Context<'_>,
//        buf: &mut [u8],
//    ) -> Poll<io::Result<usize>> {
//        log::debug!("read buf BEFORE {:?}", buf);
//        if buf.len() == 0 {
//            return Poll::Ready(Ok(0));
//        }

//        let fut = match self.read_fut {
//            ReadFut::Idle => {
//                // Read into a temporary buffer because `buf` has an anonymous lifetime, which can
//                // be shorter than the lifetime of `read_fut`.
//                let dc = self.data_channel.clone();
//                let mut temp_buf = vec![0; self.read_buf_cap];
//                self.read_fut = ReadFut::Reading(Box::pin(async move {
//                    dc.read(temp_buf.as_mut_slice()).await.map(|n| {
//                        temp_buf.truncate(n);
//                        temp_buf
//                    })
//                }));
//                self.read_fut.get_reading_mut()
//            },
//            ReadFut::Reading(ref mut fut) => fut,
//            ReadFut::RemainingData(ref mut data) => {
//                let remaining = buf.len();
//                let len = std::cmp::min(data.len(), remaining);
//                buf.copy_from_slice(&data[..len]);
//                if data.len() > remaining {
//                    // ReadFut remains to be RemainingData
//                    data.drain(0..len);
//                } else {
//                    self.read_fut = ReadFut::Idle;
//                }
//                log::debug!("read buf AFTER {:?}", buf);
//                return Poll::Ready(Ok(len));
//            },
//        };

//        loop {
//            match ready!(fut.as_mut().poll(cx)) {
//                // Retry immediately upon empty data or incomplete chunks
//                // since there's no way to setup a waker.
//                Err(Error::Sctp(webrtc_sctp::Error::ErrTryAgain)) => {},
//                // EOF has been reached => don't touch buf and just return Ok
//                Err(Error::Sctp(webrtc_sctp::Error::ErrEof)) => {
//                    self.read_fut = ReadFut::Idle;
//                    return Poll::Ready(Ok(0));
//                },
//                Err(e) => {
//                    self.read_fut = ReadFut::Idle;
//                    return Poll::Ready(Err(webrtc_error_to_io(e)));
//                },
//                Ok(mut temp_buf) => {
//                    let remaining = buf.len();
//                    let len = std::cmp::min(temp_buf.len(), remaining);
//                    buf.copy_from_slice(&temp_buf[..len]);
//                    if temp_buf.len() > remaining {
//                        temp_buf.drain(0..len);
//                        self.read_fut = ReadFut::RemainingData(temp_buf);
//                    } else {
//                        self.read_fut = ReadFut::Idle;
//                    }
//                    log::debug!("read buf AFTER {:?}", buf);
//                    return Poll::Ready(Ok(len));
//                },
//            }
//        }
//    }
//}

//impl AsyncWrite for PollDataChannel {
//    fn poll_write(
//        mut self: Pin<&mut Self>,
//        cx: &mut Context<'_>,
//        buf: &[u8],
//    ) -> Poll<io::Result<usize>> {
//        let (fut, fut_is_new) = match self.write_fut.as_mut() {
//            Some(fut) => (fut, false),
//            None => {
//                let dc = self.data_channel.clone();
//                let bytes = Bytes::copy_from_slice(buf);
//                (
//                    self.write_fut
//                        .get_or_insert(Box::pin(async move { dc.write(&bytes).await })),
//                    true,
//                )
//            },
//        };

//        match fut.as_mut().poll(cx) {
//            Poll::Pending => {
//                // If it's the first time we're polling the future, `Poll::Pending` can't be
//                // returned because that would mean the `PollDataChannel` is not ready for writing. And
//                // this is not true since we've just created a future, which is going to write the
//                // buf to the underlying dc.
//                //
//                // It's okay to return `Poll::Ready` if the data is buffered (this is what the
//                // buffered writer and `File` do).
//                if fut_is_new {
//                    Poll::Ready(Ok(buf.len()))
//                } else {
//                    // If it's the subsequent poll, it's okay to return `Poll::Pending` as it
//                    // indicates that the `PollDataChannel` is not ready for writing. Only one future
//                    // can be in progress at the time.
//                    Poll::Pending
//                }
//            },
//            Poll::Ready(Err(e)) => {
//                self.write_fut = None;
//                Poll::Ready(Err(webrtc_error_to_io(e)))
//            },
//            Poll::Ready(Ok(n)) => {
//                self.write_fut = None;
//                Poll::Ready(Ok(n))
//            },
//        }
//    }

//    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
//        match self.write_fut.as_mut() {
//            Some(fut) => match ready!(fut.as_mut().poll(cx)) {
//                Err(e) => {
//                    self.write_fut = None;
//                    Poll::Ready(Err(webrtc_error_to_io(e)))
//                },
//                Ok(_) => {
//                    self.write_fut = None;
//                    Poll::Ready(Ok(()))
//                },
//            },
//            None => Poll::Ready(Ok(())),
//        }
//    }

//    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
//        let fut = match self.shutdown_fut.as_mut() {
//            Some(fut) => fut,
//            None => {
//                let data_channel = self.data_channel.clone();
//                self.shutdown_fut
//                    .get_or_insert(Box::pin(async move { data_channel.close().await }))
//            },
//        };

//        match ready!(fut.as_mut().poll(cx)) {
//            Err(e) => Poll::Ready(Err(webrtc_error_to_io(e))),
//            Ok(_) => Poll::Ready(Ok(())),
//        }
//    }
//}

//impl Clone for PollDataChannel {
//    fn clone(&self) -> PollDataChannel {
//        PollDataChannel::new(self.clone_inner())
//    }
//}

//impl fmt::Debug for PollDataChannel {
//    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//        f.debug_struct("PollDataChannel")
//            .field("data_channel", &self.data_channel)
//            .finish()
//    }
//}

//impl AsRef<DataChannel> for PollDataChannel {
//    fn as_ref(&self) -> &DataChannel {
//        &*self.data_channel
//    }
//}

//fn webrtc_error_to_io(error: Error) -> io::Error {
//    match error {
//        e @ Error::Sctp(webrtc_sctp::Error::ErrEof) => {
//            io::Error::new(io::ErrorKind::UnexpectedEof, e.to_string())
//        },
//        e @ Error::ErrStreamClosed => {
//            io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string())
//        },
//        e => io::Error::new(io::ErrorKind::Other, e.to_string()),
//    }
//}
