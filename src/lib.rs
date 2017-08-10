extern crate byteorder;
extern crate futures;
extern crate tokio_core;
extern crate ring;
extern crate rand;
extern crate mio;
#[macro_use] extern crate log;

mod codec;

use std::io;
use std::io::Error;
use std::io::Result;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;

use rand::Rng;

use tokio_core::net::UdpCodec;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;
use tokio_core::reactor::Timeout;

use futures::stream::Stream;
use futures::sink::Sink;
use futures::IntoFuture;

use futures::Future;
use futures::Flatten;
use futures::Poll;
use futures::Async;
use futures::future::FutureResult;
use futures::future::ok;
use futures::future::err;

use codec::Response;
use codec::BindRequest;
use codec::ChangeRequest;

#[derive(Debug, PartialEq)]
pub enum Connectivity {
    OpenInternet(SocketAddr),
    FullConeNat(SocketAddr),
    SymmetricNat,
    RestrictedPortNat(SocketAddr),
    RestrictedConeNat(SocketAddr),
    SymmetricFirewall(SocketAddr),
    UdpBlocked,
}

impl Into<Option<SocketAddr>> for Connectivity {
    fn into(self) -> Option<SocketAddr> {
        match self {
            Connectivity::OpenInternet(addr) => Some(addr),
            Connectivity::FullConeNat(addr) => Some(addr),
            Connectivity::SymmetricNat => None,
            Connectivity::RestrictedPortNat(addr) => Some(addr),
            Connectivity::RestrictedConeNat(addr) => Some(addr),
            Connectivity::SymmetricFirewall(addr) => Some(addr),
            Connectivity::UdpBlocked => None,
        }
    }
}

struct Request<S,T> {
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
    fn new(sink:    S,
           stream:  T,
           dst:     SocketAddr,
           request: codec::Request,
           timeout: Result<Timeout>)
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

fn unreachable_to_udp_blocked<S,T>(tuple: (S, T, Error)) ->
    Box<Future<Item=(S, T, Connectivity),
               Error=(S, T, Error)>>
    where S: Sink<SinkItem=(Vec<u8>, SocketAddr), SinkError=io::Error> + 'static,
          T: Stream<Item=(Vec<u8>, SocketAddr), Error=io::Error> + 'static
{
    let (sink, stream, e) = tuple;

    if e.raw_os_error() == Some(101) {
        // Network unreachable
        Box::new(ok((sink, stream, Connectivity::UdpBlocked)))
    } else {
        Box::new(err((sink, stream, e)))
    }
}

struct Codec;

impl UdpCodec for Codec {
    type In = (Vec<u8>, SocketAddr);
    type Out = (Vec<u8>, SocketAddr);

    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> Result<Self::In> {
        Ok((buf.to_vec(), *src))
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> SocketAddr {
        buf.append(&mut msg.0.clone());
        msg.1
    }
}

pub fn stun3489(bind_addr: SocketAddr,
                stun_server: SocketAddr,
                handle: &Handle,
                timeout: Duration)
    -> Box<Future<Item=Connectivity, Error=io::Error>>
{
    let sock = match UdpSocket::bind(&bind_addr.clone(), handle) {
        Ok(s) => s,
        Err(e) => return Box::new(err(e)),
    };

    let bind_addr = match sock.local_addr() {
        Ok(a) => a,
        Err(e) => return Box::new(err(e)),
    };

    let (sink, stream) = sock.framed(Codec).split();

    Box::new(stun3489_generic(Box::new(sink), stream.boxed(), bind_addr, stun_server, handle, timeout)
        .map(|(_,_,c)| c)
        .map_err(|(_,_,e)| e))
}

type ConnectivityFuture<S,T> = Box<Future<Item=(S, T, Connectivity),
                                          Error=(S, T, io::Error)>>;

pub fn stun3489_generic<S,T>(sink: S,
                        stream: T,
                        bind_addr: SocketAddr,
                        stun_server: SocketAddr,
                        handle: &Handle,
                        timeout: Duration)
    -> ConnectivityFuture<S,T>
    where S: Sink<SinkItem=(Vec<u8>, SocketAddr), SinkError=io::Error> + 'static,
          T: Stream<Item=(Vec<u8>, SocketAddr), Error=io::Error> + 'static
{
    let req = codec::Request::Bind(BindRequest::default());
    let req_same_ip_same_port = req.clone();
    let req_same_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::Port), ..BindRequest::default() });
    let req_diff_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::IpAndPort), ..BindRequest::default() });

    let halt = Timeout::new(timeout, handle);
    let handle = handle.clone();

    let request1 = Request::new(sink, stream, stun_server.clone(), req, halt);

    let result = request1.and_then(move |(sink, stream, response)| {
        //println!("response1={:?}", response);
        if let Some(Response::Bind(response)) = response {
            let halt = Timeout::new(timeout, &handle);
            let request2 = Request::new(sink, stream, stun_server.clone(), req_diff_ip_diff_port, halt);

            let public_addr = response.mapped_address;

            if bind_addr.ip() == public_addr.ip() {
                // No NAT
                return Box::new(request2.and_then(move |(sink, stream, response)| {
                    //println!("response2a={:?}", response);
                    if response.is_some() {
                        return Box::new(ok((sink, stream, Connectivity::OpenInternet(public_addr))));
                    } else {
                        return Box::new(ok((sink, stream, Connectivity::SymmetricFirewall(public_addr))));
                    }
                 })) as ConnectivityFuture<S,T>
            }

            // NAT detected
            Box::new(request2.and_then(move |(sink, stream, response)| {
                //println!("response2b={:?}", response);
                if response.is_some() {
                    return Box::new(ok((sink, stream, Connectivity::FullConeNat(public_addr)))) as ConnectivityFuture<S,T>;
                }

                let halt = Timeout::new(timeout, &handle);
                let request3 = Request::new(sink, stream, stun_server.clone(), req_same_ip_same_port, halt);
                Box::new(request3.and_then(move |(sink, stream, response)| {
                    //println!("response3={:?}", response);
                    if let Some(Response::Bind(response)) = response {
                        if public_addr.ip() != response.mapped_address.ip() {
                            return Box::new(ok((sink, stream, Connectivity::SymmetricNat))) as ConnectivityFuture<S,T>;
                        }

                        let halt = Timeout::new(timeout, &handle);
                        let request4 = Request::new(sink, stream, stun_server.clone(), req_same_ip_diff_port, halt);
                        Box::new(request4.and_then(move |(sink, stream, response)| {
                            if response.is_some() {
                                Box::new(ok((sink, stream, Connectivity::RestrictedConeNat(public_addr))))
                            } else {
                                Box::new(ok((sink, stream, Connectivity::RestrictedPortNat(public_addr))))
                            }
                        }).or_else(unreachable_to_udp_blocked)) as ConnectivityFuture<S,T>
                    } else {
                         let msg = format!("Did not receive Some(BindResponse) but got {:?} instead!", response);
                        Box::new(err((sink, stream, Error::new(ErrorKind::InvalidData, msg)))) as ConnectivityFuture<S,T>
                    }
                }).or_else(unreachable_to_udp_blocked)) as ConnectivityFuture<S,T>
            }).or_else(unreachable_to_udp_blocked)) as ConnectivityFuture<S,T>
         } else {
             Box::new(ok((sink, stream, Connectivity::UdpBlocked))) as ConnectivityFuture<S,T>
         }
    }).or_else(unreachable_to_udp_blocked);

    Box::new(result)
}


#[cfg(test)]
mod tests {
    use std::time::Duration;
    use tokio_core::reactor::Core;

    use super::stun3489;
    use super::Connectivity;

    #[test]
    fn it_works() {
        let addr = "0.0.0.0:0".parse().unwrap();
        let server = "217.10.68.152:3478".parse().unwrap(); // stun.sipgate.net
        let timeout = Duration::from_secs(1);

        let mut core = Core::new().unwrap();
        let handle = core.handle();

        let conn = stun3489(addr, server, &handle, timeout);
        let result = core.run(conn);
        assert!(result.is_ok());
        assert_ne!(result.unwrap(), Connectivity::UdpBlocked);
    }

    #[test]
    fn loopback_gives_error() {
        let addr = "127.0.0.1:0".parse().unwrap();
        let server = "217.10.68.152:3478".parse().unwrap(); // stun.sipgate.net
        let timeout = Duration::from_secs(1);

        let mut core = Core::new().unwrap();
        let handle = core.handle();

        let conn = stun3489(addr, server, &handle, timeout);
        let result = core.run(conn);
        println!("{:?}", result);
        assert!(result.is_err());
    }

    #[test]
    fn no_server_gives_blocked() {
        let addr = "0.0.0.0:0".parse().unwrap();
        let server = "240.0.0.1:3478".parse().unwrap();
        let timeout = Duration::from_secs(1);

        let mut core = Core::new().unwrap();
        let handle = core.handle();

        let conn = stun3489(addr, server, &handle, timeout);
        let result = core.run(conn);
        println!("{:?}", result);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Connectivity::UdpBlocked);
    }
}

