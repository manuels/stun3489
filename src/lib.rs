extern crate byteorder;
extern crate futures;
extern crate tokio_core;
extern crate ring;
extern crate rand;

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
use futures::stream::BoxStream;
use futures::sink::Sink;
use futures::sink::BoxSink;
use futures::IntoFuture;

use futures::Future;
use futures::Flatten;
use futures::BoxFuture;
use futures::Poll;
use futures::Async;
use futures::future::FutureResult;
use futures::future::ok;
use futures::future::err;

use codec::Response;
use codec::BindRequest;
use codec::ChangeRequest;

#[derive(Debug, PartialEq)]
pub enum Connection {
    OpenInternet(SocketAddr),
    FullConeNat(SocketAddr),
    SymmetricNat,
    RestrictedPortNat(SocketAddr),
    RestrictedConeNat(SocketAddr),
    SymmetricFirewall(SocketAddr),
    UdpBlocked,
}

type IoStream<T> = BoxStream<T, Error>;
type IoSink<T> = BoxSink<T, Error>;

struct Request {
    id:      u64,
    stream:  Option<IoStream<(Vec<u8>, SocketAddr)>>,
    sink:    Option<IoSink<(Vec<u8>, SocketAddr)>>,
    codec:   codec::StunCodec,
    addr:    SocketAddr,
    sent:    bool,
    request: codec::Request,
    timeout: Flatten<FutureResult<Timeout, Error>>,
}

impl Request {
    fn new(stream:  IoStream<(Vec<u8>, SocketAddr)>,
           sink:    IoSink<(Vec<u8>, SocketAddr)>,
           dst:     SocketAddr,
           request: codec::Request,
           timeout: Result<Timeout>)
       -> Self
    {
        let mut rng = rand::thread_rng();

        Request {
            stream:  Some(stream),
            sink:    Some(sink),
            addr:    dst,
            request: request,
            timeout: timeout.into_future().flatten(),
            sent:    false,
            id:      rng.gen::<u64>(),
            codec:   codec::StunCodec::new(),
        }
    }

    pub fn mut_sink(&mut self) -> &mut IoSink<(Vec<u8>, SocketAddr)> {
        self.sink.as_mut().expect("Attempted Request::mut_sink after completion")
    }

    pub fn mut_stream(&mut self) -> &mut IoStream<(Vec<u8>, SocketAddr)> {
        self.stream.as_mut().expect("Attempted Request::mut_stream after completion")
    }
}

impl Future for Request {
    type Item = (IoStream<(Vec<u8>, SocketAddr)>, IoSink<(Vec<u8>, SocketAddr)>, Option<Response>);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if !self.sent {
            let msg = (self.id, self.addr, self.request.clone());

            let mut buf = Vec::with_capacity(2048);
            let dst = self.codec.encode(msg, &mut buf);
            assert_eq!(dst, self.addr);

            if self.mut_sink().send((buf, dst)).poll()?.is_ready() {
                self.sent = true;
            }
        }

        while let Async::Ready(Some((buf, src))) = self.mut_stream().poll()? {
            if let Ok((id, response)) = self.codec.decode(&src, &buf[..]) {
                if self.id == id {
                    return Ok(Async::Ready((self.stream.take().unwrap(),
                                            self.sink.take().unwrap(),
                                            Some(response))));
                }
            }
        }

        if self.timeout.poll()?.is_ready() {
            return Ok(Async::Ready((self.stream.take().unwrap(),
                                    self.sink.take().unwrap(),
                                    None)));
        }

        return Ok(Async::NotReady)
    }
}

fn unreachable_to_udp_blocked(e: Error) -> BoxFuture<Connection, Error> {
    if e.raw_os_error() == Some(101) {
        // Network unreachable
        ok(Connection::UdpBlocked).boxed()
    } else {
        err(e).boxed()
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

pub fn stun3489(addr: SocketAddr,
                stun_server: SocketAddr,
                handle: &Handle,
                timeout: Duration)
    -> BoxFuture<Connection, io::Error>
{
    let sock = match UdpSocket::bind(&addr.clone(), handle) {
        Ok(s) => s,
        Err(e) => return err(e).boxed(),
    };

    let addr = match sock.local_addr() {
        Ok(a) => a,
        Err(e) => return err(e).boxed(),
    };

    let (sink, stream) = sock.framed(Codec).split();

    stun3489_generic(stream.boxed(), Box::new(sink), addr, stun_server, handle, timeout)
}


pub fn stun3489_generic(stream: IoStream<(Vec<u8>, SocketAddr)>,
                        sink: IoSink<(Vec<u8>, SocketAddr)>,
                        addr: SocketAddr,
                        stun_server: SocketAddr,
                        handle: &Handle,
                        timeout: Duration)
    -> BoxFuture<Connection, io::Error>
{
    let req = codec::Request::Bind(BindRequest::default());
    let req_same_ip_same_port = req.clone();
    let req_same_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::Port), ..BindRequest::default() });
    let req_diff_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::IpAndPort), ..BindRequest::default() });

    let timeout1 = Timeout::new(timeout, handle);
    let timeout2 = Timeout::new(timeout, handle);
    let timeout3 = Timeout::new(timeout, handle);
    let timeout4 = Timeout::new(timeout, handle);

    let request1 = Request::new(stream, sink, stun_server.clone(), req, timeout1);

    let result = request1.and_then(move |(stream, sink, response)| {
        //println!("response1={:?}", response);
        if let Some(Response::Bind(response)) = response {
            let request2 = Request::new(stream, sink, stun_server.clone(), req_diff_ip_diff_port, timeout2);

            let public_addr = response.mapped_address;

            if addr.ip() == public_addr.ip() {
                // No NAT
                return request2.and_then(move |(_, _, response)| {
                    //println!("response2a={:?}", response);
                    if response.is_some() {
                        return ok(Connection::OpenInternet(public_addr)).boxed();
                    } else {
                        return ok(Connection::SymmetricFirewall(public_addr)).boxed();
                    }
                 }).boxed()
            }

             // NAT detected
            request2.and_then(move |(stream, sink, response)| {
                //println!("response2b={:?}", response);
                if response.is_some() {
                    return ok(Connection::FullConeNat(public_addr)).boxed();
                }

                let request3 = Request::new(stream, sink, stun_server.clone(), req_same_ip_same_port, timeout3);
                request3.and_then(move |(stream, sink, response)| {
                    //println!("response3={:?}", response);
                    if let Some(Response::Bind(response)) = response {
                        if public_addr.ip() != response.mapped_address.ip() {
                            return ok(Connection::SymmetricNat).boxed();
                        }

                        let request4 = Request::new(stream, sink, stun_server.clone(), req_same_ip_diff_port, timeout4);
                        request4.and_then(move |(_, _, response)| {
                            if response.is_some() {
                                ok(Connection::RestrictedConeNat(public_addr)).boxed()
                            } else {
                                ok(Connection::RestrictedPortNat(public_addr)).boxed()
                            }
                        }).or_else(unreachable_to_udp_blocked).boxed()
                    } else {
                        let msg = format!("Did not receive Some(BindResponse) but got {:?} instead!", response);
                        err(Error::new(ErrorKind::InvalidData, msg)).boxed()
                    }
                }).or_else(unreachable_to_udp_blocked).boxed()
            }).or_else(unreachable_to_udp_blocked).boxed()
         } else {
             ok(Connection::UdpBlocked).boxed()
         }
    }).or_else(unreachable_to_udp_blocked);

    result.boxed()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use tokio_core::reactor::Core;

    use super::stun3489;
    use super::Connection;

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
        assert_ne!(result.unwrap(), Connection::UdpBlocked);
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
        assert_eq!(result.unwrap(), Connection::UdpBlocked);
    }
}

