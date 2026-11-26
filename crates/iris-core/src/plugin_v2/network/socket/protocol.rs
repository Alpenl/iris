use super::super::{duplex::Wire, raw};
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};
use std::io::{self, Read, Write};
use tungstenite::{Message, WebSocket, client::IntoClientRequest, protocol::WebSocketConfig};
use url::Url;

pub(super) enum Protocol {
    Tcp(Wire),
    Ws(Box<WebSocket<Wire>>),
}
pub(super) struct Data {
    pub bytes: Vec<u8>,
    pub text: bool,
    pub eof: bool,
}
pub(super) enum ReadStep {
    Data(Data),
    Control,
    Pending,
}

impl Protocol {
    pub(super) fn new(
        wire: Wire,
        url: &Url,
        headers: &[(String, String)],
        ws: bool,
    ) -> Result<Self, ErrorCode> {
        if !ws {
            return Ok(Self::Tcp(wire));
        }
        raw::validate_headers(headers)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| ErrorCode::InvalidArgument)?;
        for (name, value) in headers {
            if name.to_ascii_lowercase().starts_with("sec-websocket-") {
                return Err(ErrorCode::Denied);
            }
            let name = tungstenite::http::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ErrorCode::InvalidArgument)?;
            let bytes: Vec<u8> = value.chars().map(|c| c as u8).collect();
            let value = tungstenite::http::HeaderValue::from_bytes(&bytes)
                .map_err(|_| ErrorCode::InvalidArgument)?;
            request.headers_mut().append(name, value);
        }
        let config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0)
            .max_write_buffer_size(CHUNK_LIMIT + 65536)
            .max_message_size(Some(CHUNK_LIMIT))
            .max_frame_size(Some(CHUNK_LIMIT));
        // This API never resolves or redirects: it uses only the authorized Wire.
        let (socket, _) = tungstenite::client::client_with_config(request, wire, Some(config))
            .map_err(|e| match e {
                tungstenite::HandshakeError::Failure(e) => ws_error(e),
                _ => ErrorCode::IoError,
            })?;
        Ok(Self::Ws(Box::new(socket)))
    }
    pub(super) fn wire(&mut self) -> &mut Wire {
        match self {
            Self::Tcp(s) => s,
            Self::Ws(s) => s.get_mut(),
        }
    }
    pub(super) fn read(&mut self, len: usize) -> Result<ReadStep, ErrorCode> {
        match self {
            Self::Tcp(s) => {
                let mut bytes = vec![0; len];
                match s.read(&mut bytes) {
                    Ok(n) => {
                        bytes.truncate(n);
                        Ok(ReadStep::Data(Data {
                            bytes,
                            text: false,
                            eof: n == 0,
                        }))
                    }
                    Err(e) if pending(&e) => Ok(ReadStep::Pending),
                    Err(e) => Err(io_error(e)),
                }
            }
            Self::Ws(s) => match s.read() {
                Ok(Message::Text(t)) => Ok(ReadStep::Data(Data {
                    bytes: t.as_bytes().to_vec(),
                    text: true,
                    eof: false,
                })),
                Ok(Message::Binary(b)) => Ok(ReadStep::Data(Data {
                    bytes: b.to_vec(),
                    text: false,
                    eof: false,
                })),
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) => {
                    Ok(ReadStep::Data(Data {
                        bytes: Vec::new(),
                        text: false,
                        eof: true,
                    }))
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => Ok(ReadStep::Control),
                Ok(_) => Err(ErrorCode::IoError),
                Err(tungstenite::Error::Io(e)) if pending(&e) => Ok(ReadStep::Pending),
                Err(e) => Err(ws_error(e)),
            },
        }
    }
    /// The offset advances only for accepted plaintext. WS WouldBlock means its
    /// single frame is already buffered; retry flush, never enqueue it twice.
    pub(super) fn write(
        &mut self,
        bytes: &[u8],
        text: bool,
        offset: &mut usize,
        started: &mut bool,
    ) -> Result<bool, ErrorCode> {
        match self {
            Self::Tcp(s) => {
                while *offset < bytes.len() {
                    match s.write(&bytes[*offset..bytes.len().min(*offset + 65536)]) {
                        Ok(0) => return Err(ErrorCode::IoError),
                        Ok(n) => *offset += n,
                        Err(e) if pending(&e) => return Ok(false),
                        Err(e) => return Err(io_error(e)),
                    }
                }
                match s.flush() {
                    Ok(()) => Ok(true),
                    Err(e) if pending(&e) => Ok(false),
                    Err(e) => Err(io_error(e)),
                }
            }
            Self::Ws(s) => {
                if !*started {
                    let message = if text {
                        Message::Text(
                            std::str::from_utf8(bytes)
                                .map_err(|_| ErrorCode::InvalidArgument)?
                                .to_owned()
                                .into(),
                        )
                    } else {
                        Message::Binary(bytes.to_vec().into())
                    };
                    match s.write(message) {
                        Ok(()) => {}
                        Err(tungstenite::Error::Io(e)) if pending(&e) => {}
                        Err(e) => return Err(ws_error(e)),
                    }
                    *started = true;
                    *offset = bytes.len();
                }
                self.flush()
            }
        }
    }
    pub(super) fn flush(&mut self) -> Result<bool, ErrorCode> {
        match self {
            Self::Tcp(s) => match s.flush() {
                Ok(()) => Ok(true),
                Err(e) if pending(&e) => Ok(false),
                Err(e) => Err(io_error(e)),
            },
            Self::Ws(s) => match s.flush() {
                Ok(()) | Err(tungstenite::Error::ConnectionClosed) => Ok(true),
                Err(tungstenite::Error::Io(e)) if pending(&e) => Ok(false),
                Err(e) => Err(ws_error(e)),
            },
        }
    }
}
fn pending(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
fn io_error(error: io::Error) -> ErrorCode {
    if error.kind() == io::ErrorKind::TimedOut {
        ErrorCode::Timeout
    } else {
        ErrorCode::IoError
    }
}
fn ws_error(error: tungstenite::Error) -> ErrorCode {
    match error {
        tungstenite::Error::Capacity(_) | tungstenite::Error::WriteBufferFull(_) => {
            ErrorCode::QuotaExceeded
        }
        tungstenite::Error::Io(e) => io_error(e),
        _ => ErrorCode::IoError,
    }
}
