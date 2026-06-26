//! Transcode-diagnosis types for `plex.transcode_health`.
//!
//! These mirror the slice of Plex's `/status/sessions` `MediaContainer`
//! the diagnosis needs. They are *not* the progenitor-generated client types:
//! progenitor's types don't derive `JsonSchema`, so they cannot cross an
//! `#[orca_tool]` boundary. Modeling the diagnosis surface as its own typed
//! structs (deriving serde + schemars) is the canonical pattern — the tool
//! deserializes the upstream `/status/sessions` JSON into these.
//!
//! ## HW-vs-software signal
//!
//! Plex reports transcode hardware state per active session on the embedded
//! `TranscodeSession`:
//!   - `transcodeHwFullPipeline` — true only when BOTH decode and encode run
//!     on hardware (the fully-offloaded path). This is the authoritative
//!     "real HW transcode" flag.
//!   - `transcodeHwDecoding` / `transcodeHwEncoding` — per-stage HW state.
//!     The vendored spec (`info.version` 1.1.1) only models
//!     `transcodeHwFullPipeline` + `transcodeHwRequested`, but live servers
//!     emit the per-stage flags too, so they are modeled `#[serde(default)]`
//!     and surfaced when present.
//!   - `videoDecision` — `transcode` / `copy` / `directplay`. A `transcode`
//!     decision with no hardware pipeline is a SOFTWARE (CPU) transcode — the
//!     condition operators chase.

use plugin_toolkit::prelude::*;

/// The `/status/sessions` envelope: `{ "MediaContainer": { "Metadata": [...] } }`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionsEnvelope {
    #[serde(rename = "MediaContainer", default)]
    pub media_container: RawMediaContainer,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawMediaContainer {
    #[serde(rename = "size", default)]
    pub size: Option<i64>,
    #[serde(rename = "Metadata", default)]
    pub metadata: Vec<RawSessionItem>,
}

/// One now-playing item in `/status/sessions`, narrowed to transcode fields.
#[derive(Debug, Clone, Deserialize)]
pub struct RawSessionItem {
    #[serde(rename = "sessionKey", default)]
    pub session_key: Option<String>,
    #[serde(rename = "title", default)]
    pub title: Option<String>,
    #[serde(rename = "User", default)]
    pub user: Option<RawUser>,
    #[serde(rename = "Player", default)]
    pub player: Option<RawPlayer>,
    #[serde(rename = "Media", default)]
    pub media: Vec<RawMedia>,
    #[serde(rename = "TranscodeSession", default)]
    pub transcode_session: Option<RawTranscodeSession>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawUser {
    #[serde(rename = "title", default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawPlayer {
    #[serde(rename = "title", default)]
    pub title: Option<String>,
    #[serde(rename = "product", default)]
    pub product: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawMedia {
    /// Per-`Media` decision Plex sometimes attaches; usually carried on `Part`.
    #[serde(rename = "videoDecision", default)]
    pub video_decision: Option<String>,
    #[serde(rename = "Part", default)]
    pub part: Vec<RawPart>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawPart {
    #[serde(rename = "decision", default)]
    pub decision: Option<String>,
    #[serde(rename = "Stream", default)]
    pub stream: Vec<RawStream>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawStream {
    /// `1` = video, `2` = audio, `3` = subtitle.
    #[serde(rename = "streamType", default)]
    pub stream_type: Option<i64>,
    #[serde(rename = "decision", default)]
    pub decision: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTranscodeSession {
    #[serde(rename = "videoDecision", default)]
    pub video_decision: Option<String>,
    #[serde(rename = "audioDecision", default)]
    pub audio_decision: Option<String>,
    #[serde(rename = "transcodeHwFullPipeline", default)]
    pub transcode_hw_full_pipeline: Option<bool>,
    #[serde(rename = "transcodeHwRequested", default)]
    pub transcode_hw_requested: Option<bool>,
    #[serde(rename = "transcodeHwDecoding", default)]
    pub transcode_hw_decoding: Option<String>,
    #[serde(rename = "transcodeHwEncoding", default)]
    pub transcode_hw_encoding: Option<String>,
    #[serde(rename = "protocol", default)]
    pub protocol: Option<String>,
    #[serde(rename = "throttled", default)]
    pub throttled: Option<bool>,
}

/// Per-session transcode health, as returned by `plex.transcode_health`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[schemars(crate = "plugin_toolkit::schemars")]
#[serde(rename_all = "camelCase")]
pub struct SessionTranscodeHealth {
    /// Plex session key, if reported.
    pub session_key: Option<String>,
    /// Title currently playing.
    pub title: Option<String>,
    /// Streaming user.
    pub user: Option<String>,
    /// Player / device name.
    pub player: Option<String>,
    /// Player product (app).
    pub product: Option<String>,
    /// True when this session has an active `TranscodeSession` (transcoding).
    pub is_transcoding: bool,
    /// Plex's video decision (`transcode` / `copy` / `directplay`).
    pub video_decision: Option<String>,
    /// Plex's audio decision.
    pub audio_decision: Option<String>,
    /// True only when the full decode+encode pipeline ran on hardware.
    pub transcode_hw_full_pipeline: bool,
    /// True when HW was requested for this transcode.
    pub transcode_hw_requested: bool,
    /// Per-stage HW decode flag, when the server reports it.
    pub transcode_hw_decoding: Option<String>,
    /// Per-stage HW encode flag, when the server reports it.
    pub transcode_hw_encoding: Option<String>,
    /// **Core flag.** True when the video is being transcoded on the CPU
    /// (software fallback) instead of through the hardware pipeline.
    pub software_fallback: bool,
}

impl SessionTranscodeHealth {
    /// Classify one `/status/sessions` item. A session with no
    /// `TranscodeSession` is direct-playing (not transcoding, no fallback). A
    /// transcoding session whose video is decided `transcode` but whose
    /// hardware pipeline is not fully engaged is a software fallback.
    pub fn from_raw(item: RawSessionItem) -> Self {
        let user = item.user.and_then(|u| u.title);
        let (player, product) = item
            .player
            .map(|p| (p.title, p.product))
            .unwrap_or((None, None));

        // The video decision can live on the TranscodeSession, the Media node,
        // or the video Stream's `decision`. Prefer the most authoritative.
        let stream_video_decision = item.media.iter().find_map(|m| {
            m.part.iter().find_map(|p| {
                p.stream
                    .iter()
                    .find(|s| s.stream_type == Some(1))
                    .and_then(|s| s.decision.clone())
            })
        });
        let media_video_decision = item.media.iter().find_map(|m| m.video_decision.clone());

        match item.transcode_session {
            None => Self {
                session_key: item.session_key,
                title: item.title,
                user,
                player,
                product,
                is_transcoding: false,
                video_decision: media_video_decision.or(stream_video_decision),
                audio_decision: None,
                transcode_hw_full_pipeline: false,
                transcode_hw_requested: false,
                transcode_hw_decoding: None,
                transcode_hw_encoding: None,
                software_fallback: false,
            },
            Some(t) => {
                let video_decision = t
                    .video_decision
                    .clone()
                    .or(media_video_decision)
                    .or(stream_video_decision);
                let full_pipeline = t.transcode_hw_full_pipeline.unwrap_or(false);
                let video_is_transcode = video_decision
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case("transcode"));
                // Software fallback: the video stream is being transcoded but
                // the full hardware pipeline did not engage.
                let software_fallback = video_is_transcode && !full_pipeline;
                Self {
                    session_key: item.session_key,
                    title: item.title,
                    user,
                    player,
                    product,
                    is_transcoding: true,
                    video_decision,
                    audio_decision: t.audio_decision,
                    transcode_hw_full_pipeline: full_pipeline,
                    transcode_hw_requested: t.transcode_hw_requested.unwrap_or(false),
                    transcode_hw_decoding: t.transcode_hw_decoding,
                    transcode_hw_encoding: t.transcode_hw_encoding,
                    software_fallback,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(transcode: Option<RawTranscodeSession>) -> RawSessionItem {
        RawSessionItem {
            session_key: Some("k1".into()),
            title: Some("Film".into()),
            user: Some(RawUser {
                title: Some("scott".into()),
            }),
            player: Some(RawPlayer {
                title: Some("Living Room".into()),
                product: Some("Plex for Apple TV".into()),
            }),
            media: Vec::new(),
            transcode_session: transcode,
        }
    }

    fn ts(video: &str, full_pipeline: bool) -> RawTranscodeSession {
        RawTranscodeSession {
            video_decision: Some(video.into()),
            audio_decision: Some("transcode".into()),
            transcode_hw_full_pipeline: Some(full_pipeline),
            transcode_hw_requested: Some(true),
            transcode_hw_decoding: None,
            transcode_hw_encoding: None,
            protocol: Some("dash".into()),
            throttled: Some(false),
        }
    }

    #[test]
    fn hw_full_pipeline_transcode_is_not_fallback() {
        let h = SessionTranscodeHealth::from_raw(item(Some(ts("transcode", true))));
        assert!(h.is_transcoding);
        assert!(h.transcode_hw_full_pipeline);
        assert!(!h.software_fallback);
    }

    #[test]
    fn software_video_transcode_is_fallback() {
        let h = SessionTranscodeHealth::from_raw(item(Some(ts("transcode", false))));
        assert!(h.is_transcoding);
        assert!(!h.transcode_hw_full_pipeline);
        assert!(h.software_fallback);
    }

    #[test]
    fn copy_video_decision_is_not_fallback() {
        // Audio-only transcode (video copied) is not a CPU video transcode.
        let h = SessionTranscodeHealth::from_raw(item(Some(ts("copy", false))));
        assert!(h.is_transcoding);
        assert!(!h.software_fallback);
    }

    #[test]
    fn direct_play_is_neither_transcoding_nor_fallback() {
        let h = SessionTranscodeHealth::from_raw(item(None));
        assert!(!h.is_transcoding);
        assert!(!h.software_fallback);
        assert_eq!(h.title.as_deref(), Some("Film"));
        assert_eq!(h.user.as_deref(), Some("scott"));
    }

    #[test]
    fn envelope_deserializes() {
        let raw = r#"{
            "MediaContainer": {
                "size": 1,
                "Metadata": [
                    { "sessionKey": "9", "title": "Show",
                      "TranscodeSession": { "videoDecision": "transcode",
                                            "transcodeHwFullPipeline": false } }
                ]
            }
        }"#;
        let env: SessionsEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(env.media_container.metadata.len(), 1);
        let item = env.media_container.metadata.into_iter().next().unwrap();
        let h = SessionTranscodeHealth::from_raw(item);
        assert!(h.software_fallback);
    }
}
