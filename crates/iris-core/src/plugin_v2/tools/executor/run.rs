use super::*;
use crate::{
    plugin_v2::{
        files::InputAction,
        network::{TransportKind, normalize_url},
        tools::{
            arguments,
            output::{self, Extent},
            proxy::Proxy,
            source::{Data, InputRelay},
        },
    },
    process_tree,
};
use iris_api::v2::{
    task::{MediaContainer, SampleRate},
    task_result::ArtifactResult,
};
use std::time::Duration;
impl ToolExecutor {
    pub(super) fn run(
        &self,
        task: ToolTask,
        context: &TaskContext,
    ) -> Result<IoOutcome, ErrorCode> {
        match task {
            ToolTask::ExtractAudio {
                input,
                track,
                sample_rate,
            } => {
                let tool = self.tool(ToolKind::Ffmpeg, &context.cancel)?;
                tool.revalidate(&context.cancel)?;
                let input = self.0.files.consume_input(
                    &context.owner,
                    input,
                    context.generation,
                    InputAction::ExtractAudio,
                )?;
                let relay = InputRelay::open(
                    Data::File(input.file.clone()),
                    &context.owner,
                    &context.ledger,
                    context.generation,
                    &context.cancel,
                )?;
                let rate = match sample_rate {
                    SampleRate::Hz16000 => 16000,
                    SampleRate::Hz48000 => 48000,
                };
                let scratch = self.0.root.scratch_dir()?;
                let mut writer = self.0.root.writer("wav")?;
                writer.write(&wav_header(0, rate), &context.cancel)?;
                let mut command = tool.command(&scratch.path);
                command.args(arguments::audio(&relay.url, track, sample_rate)?);
                process_tree::run(
                    &mut command,
                    None,
                    &context.cancel,
                    Duration::from_secs(300),
                    &mut |bytes| writer.write(bytes, &context.cancel),
                )?;
                drop(scratch);
                input.file.unchanged()?;
                relay.close()?;
                let size = writer.len().checked_sub(44).ok_or(ErrorCode::IoError)?;
                if size == 0 || !size.is_multiple_of(2) {
                    return Err(ErrorCode::IoError);
                }
                let size = u32::try_from(size).map_err(|_| ErrorCode::QuotaExceeded)?;
                writer.patch(0, &wav_header(size, rate))?;
                let artifact = writer.finish(&context.cancel)?;
                self.publish(artifact.clone(), result(&artifact), context)
            }
            ToolTask::LosslessSlice {
                input,
                export,
                start_ms,
                end_ms,
                container,
            } => {
                let tool = self.tool(ToolKind::Ffmpeg, &context.cancel)?;
                let probe = self.tool(ToolKind::Ffprobe, &context.cancel)?;
                tool.revalidate(&context.cancel)?;
                probe.revalidate(&context.cancel)?;
                let export = self
                    .0
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exports
                    .remove(&export)
                    .ok_or(ErrorCode::StaleHandle)?;
                let input = self.0.files.consume_input(
                    &context.owner,
                    input,
                    context.generation,
                    InputAction::Slice,
                )?;
                let relay = InputRelay::open(
                    Data::File(input.file.clone()),
                    &context.owner,
                    &context.ledger,
                    context.generation,
                    &context.cancel,
                )?;
                let format = match container {
                    MediaContainer::Matroska => "mkv",
                    MediaContainer::Mp4 => "mp4",
                    MediaContainer::Webm => "webm",
                };
                let scratch = self.0.root.scratch_dir()?;
                let mut writer = self.0.root.writer(format)?;
                let mut command = tool.command(&scratch.path);
                command.args(arguments::slice(
                    &relay.url, start_ms.0, end_ms.0, container,
                )?);
                process_tree::run(
                    &mut command,
                    None,
                    &context.cancel,
                    Duration::from_secs(300),
                    &mut |bytes| writer.write(bytes, &context.cancel),
                )?;
                input.file.unchanged()?;
                relay.close()?;
                if writer.is_empty() {
                    return Err(ErrorCode::IoError);
                }
                let artifact = writer.finish(&context.cancel)?;
                let relay = InputRelay::open(
                    Data::Artifact(artifact.clone()),
                    &context.owner,
                    &context.ledger,
                    context.generation,
                    &context.cancel,
                )?;
                let mut command = probe.command(&scratch.path);
                command.args(arguments::probe(&relay.url));
                let mut extent = Extent::default();
                process_tree::run(
                    &mut command,
                    None,
                    &context.cancel,
                    Duration::from_secs(120),
                    &mut |bytes| extent.push(bytes),
                )?;
                drop(scratch);
                let (start, end) = extent.finish()?;
                relay.close()?;
                self.bind(context)?;
                export.commit(
                    &artifact,
                    &context.owner,
                    context.generation,
                    &context.ledger,
                    &context.cancel,
                )?;
                let mut value = result(&artifact);
                value.actual_start_ms = Some(start.to_string());
                value.actual_end_ms = Some(end.to_string());
                value.requested_start_ms = Some(start_ms);
                value.requested_end_ms = Some(end_ms);
                value.exported = true;
                self.publish(artifact, value, context)
            }
            ToolTask::Transcribe {
                audio,
                model,
                language,
            } => {
                let tool = self.tool(ToolKind::WhisperCpp, &context.cancel)?;
                tool.revalidate(&context.cancel)?;
                let model = self
                    .0
                    .models
                    .get(&model)
                    .cloned()
                    .map(Ok)
                    .unwrap_or_else(|| self.0.catalog.model(model, &context.cancel))?;
                let audio = self.artifact(audio, context.generation)?;
                if audio.format != "wav" || audio.size() > 64 << 20 {
                    return Err(ErrorCode::Unsupported);
                }
                let scratch = self.0.root.scratch_dir()?;
                let mut command = tool.command(&scratch.path);
                command.args(arguments::whisper(
                    &audio.path(),
                    model.path(&context.cancel)?,
                    language.as_deref(),
                )?);
                let output = process_tree::capture(
                    &mut command,
                    &context.cancel,
                    Duration::from_secs(600),
                    8 << 20,
                )?;
                drop(scratch);
                let subtitles = output::transcription(&output)?;
                let mut writer = self.0.root.writer("vtt")?;
                writer.write(&subtitles, &context.cancel)?;
                let artifact = writer.finish(&context.cancel)?;
                self.publish(artifact.clone(), result(&artifact), context)
            }
            ToolTask::ResolveMetadata { url, .. } => {
                self.0.network.request(&url, TransportKind::Http, None)?;
                let tool = self.tool(ToolKind::YtDlp, &context.cancel)?;
                tool.revalidate(&context.cancel)?;
                let proxy = Proxy::start(
                    self.0.network.clone(),
                    context.ledger.clone(),
                    &context.cancel,
                    self.0.activity.clone(),
                    &url,
                )?;
                let scratch = self.0.root.scratch_dir()?;
                let mut command = tool.command(&scratch.path);
                command.args(arguments::metadata(&url, &proxy.url));
                let output = process_tree::capture(
                    &mut command,
                    &context.cancel,
                    Duration::from_secs(120),
                    8 << 20,
                )
                .map_err(|error| {
                    if error == ErrorCode::IoError {
                        proxy.failure().unwrap_or(error)
                    } else {
                        error
                    }
                })?;
                drop(scratch);
                drop(proxy);
                let output = metadata(&output, &self.0.network)?;
                let mut writer = self.0.root.writer("json")?;
                writer.write(&output, &context.cancel)?;
                let artifact = writer.finish(&context.cancel)?;
                self.publish(artifact.clone(), result(&artifact), context)
            }
        }
    }
}
pub(super) fn result(artifact: &Artifact) -> ArtifactResult {
    ArtifactResult {
        artifact: artifact.id,
        bytes: Id(artifact.size()),
        format: artifact.format.clone(),
        actual_start_ms: None,
        actual_end_ms: None,
        requested_start_ms: None,
        requested_end_ms: None,
        exported: false,
    }
}
fn wav_header(bytes: u32, rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(bytes + 36).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    out
}
fn metadata(bytes: &[u8], network: &NetworkGrant) -> Result<Vec<u8>, ErrorCode> {
    use serde_json::{Value, json};
    let value: Value = crate::plugin_v2::json::decode_exact(bytes, 8 << 20)?;
    let object = value.as_object().ok_or(ErrorCode::InvalidArgument)?;
    if object.contains_key("entries") {
        return Err(ErrorCode::Unsupported);
    }
    let mut formats = object
        .get("formats")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if formats.is_empty() && object.contains_key("url") {
        formats.push(value.clone());
    }
    if formats.len() > 128 {
        return Err(ErrorCode::QuotaExceeded);
    }
    let had_formats = !formats.is_empty();
    let mut clean = Vec::new();
    for format in formats {
        let Some(url) = format.get("url").and_then(Value::as_str) else {
            continue;
        };
        let Ok(parsed) = normalize_url(url, TransportKind::Http) else {
            continue;
        };
        let Ok(grant) = network.request(parsed.as_str(), TransportKind::Http, None) else {
            continue;
        };
        let headers = format
            .get("http_headers")
            .or_else(|| object.get("http_headers"));
        let headers = headers
            .map(|v| {
                let map = v.as_object().ok_or(ErrorCode::InvalidArgument)?;
                map.iter()
                    .map(|(k, v)| {
                        Ok((
                            k.clone(),
                            v.as_str().ok_or(ErrorCode::InvalidArgument)?.into(),
                        ))
                    })
                    .collect::<Result<Vec<_>, ErrorCode>>()
            })
            .transpose()?
            .unwrap_or_default();
        if crate::plugin_v2::network::raw::validate_headers(&headers).is_err()
            || grant.check_credentials(&parsed, &headers).is_err()
        {
            continue;
        }
        let mut item = json!({"url":parsed.as_str(), "headers":headers});
        for key in [
            "format_id",
            "format_note",
            "ext",
            "acodec",
            "vcodec",
            "width",
            "height",
            "fps",
            "tbr",
            "language",
        ] {
            if let Some(value) = format.get(key) {
                if value.is_object()
                    || value.is_array()
                    || value
                        .as_str()
                        .is_some_and(|s| s.len() > 512 || s.chars().any(char::is_control))
                {
                    return Err(ErrorCode::InvalidArgument);
                }
                item[key] = value.clone();
            }
        }
        clean.push(item);
    }
    if had_formats && clean.is_empty() {
        return Err(ErrorCode::Denied);
    }
    let title = object
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("媒体");
    if title.len() > 1024 || title.chars().any(char::is_control) {
        return Err(ErrorCode::InvalidArgument);
    }
    crate::plugin_v2::json::encode(
        &json!({"schema":1, "title":title, "formats":clean}),
        8 << 20,
    )
}
