use super::super::duplex::Wire;
use iris_api::v2::{CHUNK_LIMIT, ErrorCode};
pub(super) use iris_net::ws_tcp::{Data, ReadStep};
use iris_net::ws_tcp::{Protocol as NetProtocol, WireError};
use url::Url;

pub(super) struct Protocol(NetProtocol);

fn map_error(error: WireError) -> ErrorCode {
    match error {
        WireError::InvalidArgument => ErrorCode::InvalidArgument,
        WireError::Denied => ErrorCode::Denied,
        WireError::QuotaExceeded => ErrorCode::QuotaExceeded,
        WireError::Timeout => ErrorCode::Timeout,
        WireError::IoError => ErrorCode::IoError,
    }
}

impl Protocol {
    pub(super) fn new(
        wire: Wire,
        url: &Url,
        headers: &[(String, String)],
        ws: bool,
    ) -> Result<Self, ErrorCode> {
        NetProtocol::new(wire, url, headers, ws, CHUNK_LIMIT)
            .map(Self)
            .map_err(map_error)
    }

    pub(super) fn wire(&mut self) -> &mut Wire {
        self.0.wire()
    }

    pub(super) fn read(&mut self, len: usize) -> Result<ReadStep, ErrorCode> {
        self.0.read(len).map_err(map_error)
    }

    pub(super) fn write(
        &mut self,
        bytes: &[u8],
        text: bool,
        offset: &mut usize,
        started: &mut bool,
    ) -> Result<bool, ErrorCode> {
        self.0
            .write(bytes, text, offset, started)
            .map_err(map_error)
    }

    pub(super) fn flush(&mut self) -> Result<bool, ErrorCode> {
        self.0.flush().map_err(map_error)
    }
}
