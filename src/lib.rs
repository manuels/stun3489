extern crate byteorder;
extern crate futures;
#[macro_use]
extern crate tokio_core;
extern crate ring;
extern crate rand;

mod codec;
mod timeout;

use std::io;
use std::io::Error;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::Duration;
use std::sync::Arc;

use rand::Rng;

use tokio_core::net::UdpCodec;
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use futures::Future;
use futures::BoxFuture;
use futures::Poll;
use futures::Async;
use futures::future::ok;
use futures::future::err;

use codec::Response;
use codec::BindRequest;
use codec::ChangeRequest;
use timeout::IntoTimeoutFuture;

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

struct Request {
    id: u64,
    socket: Arc<UdpSocket>,
    codec: codec::StunCodec,
    addr: SocketAddr,
    sent: bool,
    request: codec::Request,
}

impl Request {
    fn new(socket: Arc<UdpSocket>, dst: SocketAddr, request: codec::Request) -> Self {
        let mut rng = rand::thread_rng();

        Request {
            socket:  socket,
            addr:    dst,
            request: request,
            sent:    false,
            id:      rng.gen::<u64>(),
            codec:   codec::StunCodec::new(),
        }
    }
}

impl Future for Request {
    type Item = Response;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if !self.sent && self.socket.poll_write().is_ready() {
            let msg = (self.id, self.addr, self.request.clone());

            let mut buf = Vec::with_capacity(2048);
            let dst = self.codec.encode(msg, &mut buf);
            assert_eq!(dst, self.addr);

            self.socket.send_to(&buf[..], &self.addr)?;
            self.sent = true;
        }

        let mut buf = [0; 2048];
        while self.socket.poll_read().is_ready() {
            let (n, src) = try_nb!(self.socket.recv_from(&mut buf));

            let (id, response) = self.codec.decode(&src, &buf[..n])?;

            if self.id == id {
                return Ok(Async::Ready(response));
            }
        }

        return Ok(Async::NotReady)
    }
}

struct Stun;

fn unreachable_to_udp_blocked(e: Error) -> BoxFuture<Connection, Error> {
    if e.raw_os_error() == Some(101) {
        // Network unreachable
        ok(Connection::UdpBlocked).boxed()
    } else {
        err(e).boxed()
    }
}



pub fn stun3489(addr: SocketAddr, stun_server: SocketAddr, handle: &Handle, timeout: Duration)
    -> BoxFuture<Connection, io::Error>
{
    let sock = match UdpSocket::bind(&addr.clone(), handle) {
        Ok(s) => Arc::new(s),
        Err(e) => return err(e).boxed(),
    };

    let req = codec::Request::Bind(BindRequest::default());
    let req_same_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::Port), ..BindRequest::default() });
    let req_same_ip_same_port = req.clone();
    let req_diff_ip_diff_port = codec::Request::Bind(BindRequest { change_request: Some(ChangeRequest::IpAndPort), ..BindRequest::default() });

    let request1 = Request::new(sock.clone(), stun_server.clone(), req);
    let request2 = Request::new(sock.clone(), stun_server.clone(), req_diff_ip_diff_port);
    let request3 = Request::new(sock.clone(), stun_server.clone(), req_same_ip_same_port);
    let request4 = Request::new(sock.clone(), stun_server.clone(), req_same_ip_diff_port);

    let request1 = request1.timeout(handle, timeout);
    let request2 = request2.timeout(handle, timeout);
    let request3 = request3.timeout(handle, timeout);
    let request4 = request4.timeout(handle, timeout);

    let result = request1.and_then(move |response| {
        //println!("response1={:?}", response);
        if let Some(Response::Bind(response)) = response {
            let public_addr = response.mapped_address;

            if addr.ip() == public_addr.ip() {
                // No NAT
                return request2.and_then(move |response| {
                    println!("response2a={:?}", response);
                    if response.is_some() {
                        return ok(Connection::OpenInternet(public_addr)).boxed();
                    } else {
                        return ok(Connection::SymmetricFirewall(public_addr)).boxed();
                    }
                }).boxed()
            }

            // NAT detected
            request2.and_then(move |response| {
                //println!("response2b={:?}", response);
                if response.is_some() {
                    return ok(Connection::FullConeNat(public_addr)).boxed();
                }

                request3.and_then(move |response| {
                    //println!("response3={:?}", response);
                    if let Some(Response::Bind(response)) = response {
                        if public_addr.ip() == response.mapped_address.ip() {
                            return ok(Connection::SymmetricNat).boxed();
                        }

                        request4.and_then(move |response| {
                            if response.is_some() {
                                ok(Connection::RestrictedConeNat(public_addr)).boxed()
                            } else {
                                ok(Connection::RestrictedPortNat(public_addr)).boxed()
                            }
                        }).or_else(unreachable_to_udp_blocked).boxed()
                    } else {
                        let msg = format!("Did not receive Some(BindResponse) but {:?} instead!", response);
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
}

