use super::{Id, WireU64};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskError {
    pub code: i32,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginResult {
    pub origin: String,
    pub origin_grant: Id,
    pub insecure_auth: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreRead {
    pub store_value: Option<String>,
    pub revision: Option<Id>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreWrite {
    pub stored: bool,
    pub revision: Option<Id>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum HttpResult {
    Upload {
        stream_handle: Id,
        written: Option<WireU64>,
    },
    Response {
        stream_handle: Id,
        status: u16,
        url: String,
        headers: Vec<(String, String)>,
        written: Option<WireU64>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadResult {
    pub eof: bool,
    pub text: Option<bool>,
    pub blob: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteResult {
    pub written: WireU64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandleResult {
    pub stream_handle: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthBegin {
    pub transaction: Id,
    pub redirect_uri: String,
    pub state: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub expires_in: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthCode {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
    pub issuer: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRejected {
    pub oauth_error: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactResult {
    pub artifact: Id,
    pub bytes: WireU64,
    pub format: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactWriterResult {
    pub artifact_writer: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HashResult {
    pub algorithm: String,
    pub digest: String,
    pub bytes: WireU64,
}
