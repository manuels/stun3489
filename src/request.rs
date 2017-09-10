use std::io;
use std::io::Error;
use std::net::SocketAddr;

use rand;
use rand::Rng;

use futures::Future;
use futures::Flatten;
use futures::Poll;
use futures::Async;
use futures::future::FutureResult;

use futures::stream::Stream;
use futures::sink::Sink;
use futures::IntoFuture;

use tokio_core::net::UdpCodec;
use tokio_core::reactor::Timeout;

use codec;
use codec::Response;

pub struct Request<S,T> {
    id:      u64,
    sink:    Option<S>,
    stream:  Option<T>,
    codec:   codec::StunCodec,
    addr:    SocketAddr,
    sent:    bool,
    request: codec::Request,
    timeout: Flatten<FutureResult<Timeout, Error>>,
}

impl<S,T> Request<S,T>
    where S: Sink<SinkItem=(Vec<u8>, SocketAddr), SinkError=io::Error>,
          T: Stream<Item=(Vec<u8>, SocketAddr), Error=io::Error>
{
    pub fn new(sink:    S,
           stream:  T,
           dst:     SocketAddr,
           request: codec::Request,
           timeout: io::Result<Timeout>)
       -> Self
    {
        let mut rng = rand::thread_rng();

        Request {
            sink:    Some(sink),
            stream:  Some(stream),
            addr:    dst,
            request: request,
            timeout: timeout.into_future().flatten(),
            sent:    false,
            id:      rng.gen::<u64>(),
            codec:   codec::StunCodec::new(),
        }
    }

    pub fn mut_sink(&mut self) -> &mut S {
        self.sink.as_mut().expect("Attempted Request::mut_sink after completion")
    }

    pub fn mut_stream(&mut self) -> &mut T {
        self.stream.as_mut().expect("Attempted Request::mut_stream after completion")
    }
}

impl<S,T> Future for Request<S,T>
    where S: Sink<SinkItem=(Vec<u8>, SocketAddr), SinkError=io::Error>,
          T: Stream<Item=(Vec<u8>, SocketAddr), Error=io::Error>
{
    type Item = (S, T, Option<Response>);
    type Error = (S, T, io::Error);

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if !self.sent {
            let msg = (self.id, self.addr, self.request.clone());

            let mut buf = Vec::with_capacity(2048);
            let dst = self.codec.encode(msg, &mut buf);
            assert_eq!(dst, self.addr);

            let len = buf.len();

            match self.mut_sink().send((buf, dst)).poll() {
                Ok(Async::Ready(_)) => {
                    debug!("sent {} bytes to {:?}", len, dst);
                    self.sent = true;
                },
                Ok(Async::NotReady) => (),
                Err(e) => return Err((self.sink.take().unwrap(),
                                      self.stream.take().unwrap(),
                                      e)),
            }
        }

        loop {
            match self.mut_stream().poll() {
                Ok(Async::NotReady) => break,
                Err(e) => return Err((self.sink.take().unwrap(),
                                      self.stream.take().unwrap(),
                                      e)),
                Ok(Async::Ready(None)) => {
                    return Ok(Async::Ready((self.sink.take().unwrap(),
                                            self.stream.take().unwrap(),
                                            None)));
                },
                Ok(Async::Ready(Some((buf, src)))) => {
                    if let Ok((id, response)) = self.codec.decode(&src, &buf[..]) {
                        if self.id == id {
                            return Ok(Async::Ready((self.sink.take().unwrap(),
                                                    self.stream.take().unwrap(),
                                                    Some(response))));
                        }
                    }
                }
            }

        }

        match self.timeout.poll() {
                Err(e) => return Err((self.sink.take().unwrap(),
                                      self.stream.take().unwrap(),
                                      e)),
                Ok(Async::NotReady) => (),
                Ok(Async::Ready(_)) => {
                    return Ok(Async::Ready((self.sink.take().unwrap(),
                                            self.stream.take().unwrap(),
                                            None)));
                }
        }

        return Ok(Async::NotReady)
    }
}

