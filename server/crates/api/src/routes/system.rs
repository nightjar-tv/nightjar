use crate::state::AppState;
use axum::{Json, extract::State};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeCapabilitiesDto {
    pub ffmpeg_version: Option<String>,
    pub preferred_h264_encoder: String,
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
