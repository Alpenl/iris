//! Convert an already authorized connection into an owned nonblocking transport.
use super::{RequestGrant, raw};
use crate::vfs::http_client::{
    ConnectRegistration, HttpCancel, NetworkScope, SocketRegistration, Transport, connect_transport,
};
use iris_api::v2::ErrorCode;
use mio::{Interest, Registry, Token};
use std::{
    io::{self, Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};
use url::Url;

enum State {
    Blocking(Transport),
    Plain(mio::net::TcpStream),
    Tls(Box<(rustls::ClientConnection, mio::net::TcpStream)>),
    Empty,
}
pub(crate) struct Wire {
    state: State,
    _registration: SocketRegistration,
}
impl Wire {
    pub(crate) fn connect(
        url: &Url,
        grant: Arc<RequestGrant>,
        cancel: &HttpCancel,
    ) -> Result<Self, ErrorCode> {
        let scope =
            NetworkScope::authorized(url.clone(), grant.clone()).map_err(|_| ErrorCode::Denied)?;
        let (transport, registration) = connect_transport(
            url,
            &scope,
            cancel,
            Some(Instant::now() + Duration::from_secs(10)),
        )
        .map_err(|e| raw::transport_error(e, &grant, cancel))?;
        Ok(Self {
            state: State::Blocking(transport),
            _registration: registration,
        })
    }
    pub(crate) fn nonblocking(&mut self) -> io::Result<()> {
        self.state = match std::mem::replace(&mut self.state, State::Empty) {
            State::Blocking(Transport::Plain(socket)) => {
                let tcp = socket.into_tcp();
                tcp.set_nonblocking(true)?;
                State::Plain(mio::net::TcpStream::from_std(tcp))
            }
            State::Blocking(Transport::Tls(stream)) => {
                let (mut conn, socket) = stream.into_parts();
                let tcp = socket.into_tcp();
                conn.set_buffer_limit(Some(64 * 1024));
                tcp.set_nonblocking(true)?;
                State::Tls(Box::new((conn, mio::net::TcpStream::from_std(tcp))))
            }
            state => state,
        };
        Ok(())
    }
    fn socket(&mut self) -> io::Result<&mut mio::net::TcpStream> {
        match &mut self.state {
            State::Plain(s) => Ok(s),
            State::Tls(s) => Ok(&mut s.1),
            _ => Err(io::ErrorKind::NotConnected.into()),
        }
    }
    pub(crate) fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        registry.register(self.socket()?, token, interest)
    }
    pub(crate) fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        registry.deregister(self.socket()?)
    }
    pub(crate) fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        registry.reregister(self.socket()?, token, interest)
    }
    pub(crate) fn wants_read(&self) -> bool {
        matches!(&self.state, State::Tls(s) if s.0.wants_read())
    }
    pub(crate) fn wants_write(&self) -> bool {
        matches!(&self.state, State::Tls(s) if s.0.wants_write())
    }
}
impl Read for Wire {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.state {
            State::Blocking(s) => s.read(buf),
            State::Plain(s) => s.read(buf),
            State::Tls(s) => rustls::Stream::new(&mut s.0, &mut s.1).read(buf),
            State::Empty => Err(io::ErrorKind::NotConnected.into()),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.state {
            State::Blocking(s) => s.write(buf),
            State::Plain(s) => s.write(buf),
            State::Tls(s) => rustls::Stream::new(&mut s.0, &mut s.1).write(buf),
            State::Empty => Err(io::ErrorKind::NotConnected.into()),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match &mut self.state {
            State::Blocking(s) => s.flush(),
            State::Plain(s) => s.flush(),
            State::Tls(s) => rustls::Stream::new(&mut s.0, &mut s.1).flush(),
            State::Empty => Err(io::ErrorKind::NotConnected.into()),
        }
    }
}

pub(crate) fn cancellation_wake(
    cancel: &HttpCancel,
    wake: Arc<mio::Waker>,
) -> Result<ConnectRegistration, ErrorCode> {
    cancel
        .register_shared_wake(wake)
        .map_err(|_| ErrorCode::Cancelled)
}
