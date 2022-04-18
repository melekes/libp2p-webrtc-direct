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

use bytes::Bytes;

use futures::prelude::*;
use webrtc_data::data_channel::DataChannel;
use webrtc_data::Error;

use std::fmt;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// A wrapper around around [`DataChannel`], which implements [`AsyncRead`] and
/// [`AsyncWrite`].
///
/// Both `poll_read` and `poll_write` calls allocate temporary buffers, which results in an
/// additional overhead.
pub struct PollDataChannel<'a> {
    data_channel: Arc<DataChannel>,

    read_fut: Option<Pin<Box<dyn Future<Output = Result<Vec<u8>, Error>> + Send + 'a>>>,
    write_fut: Option<Pin<Box<dyn Future<Output = Result<usize, Error>> + Send + 'a>>>,
    shutdown_fut: Option<Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'a>>>,
}

impl PollDataChannel<'_> {
    /// Constructs a new `PollDataChannel`.
    pub fn new(data_channel: Arc<DataChannel>) -> Self {
        Self {
            data_channel,
            read_fut: None,
            write_fut: None,
            shutdown_fut: None,
        }
    }

    /// Get back the inner data_channel.
    pub fn into_inner(self) -> Arc<DataChannel> {
        self.data_channel
    }

    /// Obtain a clone of the inner data_channel.
    pub fn clone_inner(&self) -> Arc<DataChannel> {
        self.data_channel.clone()
    }
}

impl AsyncRead for PollDataChannel<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let fut = match self.read_fut.as_mut() {
            Some(fut) => fut,
            None => {
                // read into a temporary buffer because `buf` has an unonymous lifetime, which can be
                // shorter than the lifetime of `read_fut`.
                let dc = self.data_channel.clone();
                let mut temp_buf = vec![0; buf.len()];
                self.read_fut.get_or_insert(Box::pin(async move {
                    let res = dc.read(temp_buf.as_mut_slice()).await;
                    match res {
                        Ok(n) => {
                            temp_buf.truncate(n);
                            Ok(temp_buf)
                        },
                        Err(e) => Err(e),
                    }
                }))
            },
        };

        loop {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                // retry immediately upon empty data or incomplete chunks
                // since there's no way to setup a waker.
                Poll::Ready(Err(Error::Sctp(webrtc_sctp::Error::ErrTryAgain))) => {},
                // EOF has been reached => don't touch buf and just return Ok
                Poll::Ready(Err(Error::Sctp(webrtc_sctp::Error::ErrEof))) => {
                    self.read_fut = None;
                    return Poll::Ready(Ok(0));
                },
                Poll::Ready(Err(e)) => {
                    self.read_fut = None;
                    return Poll::Ready(Err(webrtc_error_to_io(e)));
                },
                Poll::Ready(Ok(read_buf)) => {
                    let len = std::cmp::min(read_buf.len(), buf.len());
                    buf.copy_from_slice(&read_buf[..len]);
                    self.read_fut = None;
                    return Poll::Ready(Ok(len));
                },
            }
        }
    }
}

impl AsyncWrite for PollDataChannel<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let (fut, fut_is_new) = match self.write_fut.as_mut() {
            Some(fut) => (fut, false),
            None => {
                let dc = self.data_channel.clone();
                let bytes = Bytes::copy_from_slice(buf);
                (
                    self.write_fut
                        .get_or_insert(Box::pin(async move { dc.write(&bytes).await })),
                    true,
                )
            },
        };

        match fut.as_mut().poll(cx) {
            Poll::Pending => {
                // If it's the first time we're polling the future, `Poll::Pending` can't be
                // returned because that would mean the `PollDataChannel` is not ready for writing. And
                // this is not true since we've just created a future, which is going to write the
                // buf to the underlying dc.
                //
                // It's okay to return `Poll::Ready` if the data is buffered (this is what the
                // buffered writer and `File` do).
                if fut_is_new {
                    Poll::Ready(Ok(buf.len()))
                } else {
                    // If it's the subsequent poll, it's okay to return `Poll::Pending` as it
                    // indicates that the `PollDataChannel` is not ready for writing. Only one future
                    // can be in progress at the time.
                    Poll::Pending
                }
            },
            Poll::Ready(Err(e)) => {
                self.write_fut = None;
                Poll::Ready(Err(webrtc_error_to_io(e)))
            },
            Poll::Ready(Ok(n)) => {
                self.write_fut = None;
                Poll::Ready(Ok(n))
            },
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.write_fut.as_mut() {
            Some(fut) => match fut.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => {
                    self.write_fut = None;
                    Poll::Ready(Err(webrtc_error_to_io(e)))
                },
                Poll::Ready(Ok(_)) => {
                    self.write_fut = None;
                    Poll::Ready(Ok(()))
                },
            },
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let fut = match self.shutdown_fut.as_mut() {
            Some(fut) => fut,
            None => {
                let dc = self.data_channel.clone();
                self.shutdown_fut
                    .get_or_insert(Box::pin(async move { dc.close().await }))
            },
        };

        match fut.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(webrtc_error_to_io(e))),
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
        }
    }
}

impl<'a> Clone for PollDataChannel<'a> {
    fn clone(&self) -> PollDataChannel<'a> {
        PollDataChannel::new(self.clone_inner())
    }
}

impl fmt::Debug for PollDataChannel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PollDataChannel")
            .field("data_channel", &self.data_channel)
            .finish()
    }
}

impl AsRef<DataChannel> for PollDataChannel<'_> {
    fn as_ref(&self) -> &DataChannel {
        &*self.data_channel
    }
}

fn webrtc_error_to_io(error: Error) -> io::Error {
    match error {
        e @ Error::Sctp(webrtc_sctp::Error::ErrEof) => {
            io::Error::new(io::ErrorKind::UnexpectedEof, e.to_string())
        },
        e @ Error::ErrStreamClosed => {
            io::Error::new(io::ErrorKind::ConnectionAborted, e.to_string())
        },
        e => io::Error::new(io::ErrorKind::Other, e.to_string()),
    }
}
