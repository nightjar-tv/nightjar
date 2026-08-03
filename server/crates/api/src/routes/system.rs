use crate::state::AppState;
use axum::{Json, extract::State};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeCapabilitiesDto {
    pub ffmpeg_version: Option<String>,
    pub preferred_h264_encoder: String,
    /// Render node or device path for the preferred encode leg, when any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_device: Option<String>,
    pub encoders: Vec<EncoderCandidateDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderCandidateDto {
    pub name: String,
    pub backend: String,
    pub status: String,
    pub reason: Option<String>,
}

pub async fn transcode_capabilities(
    State(state): State<AppState>,
) -> Json<TranscodeCapabilitiesDto> {
    let caps = &state.transcode_caps;
    Json(TranscodeCapabilitiesDto {
        ffmpeg_version: caps.ffmpeg_version.clone(),
        preferred_h264_encoder: caps.preferred_h264_encoder.clone(),
        preferred_device: caps.preferred_encode_leg.device.clone(),
        encoders: caps
            .encoders
            .iter()
            .map(|e| EncoderCandidateDto {
                name: e.name.clone(),
                backend: e.backend.clone(),
                status: e.status.as_str().to_string(),
                reason: e.reason.clone(),
            })
            .collect(),
    })
}
