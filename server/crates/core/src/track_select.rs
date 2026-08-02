//! Ranked audio/subtitle selection (ADR-0024).

/// Server default until Block 2 profiles supply a preference.
pub const DEFAULT_PREFERENCE_LANGUAGE: &str = "en";

/// One inventory row for ranking. Callers map probe/DTO fields into this.
#[derive(Debug, Clone)]
pub struct TrackCandidate {
    pub track_id: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    /// Image subtitle (PGS / VobSub). Delivery-cost tiebreak; untested until
    /// the PGS fixture covers it (ADR-0024).
    pub is_image: bool,
    pub stream_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSelection {
    pub track_id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KindBits {
    commentary: bool,
    sdh: bool,
    signs: bool,
    forced_title: bool,
}

impl KindBits {
    fn from_title(title: Option<&str>) -> Self {
        let t = title.unwrap_or("").to_ascii_lowercase();
        Self {
            commentary: contains_token(
                &t,
                &[
                    "commentary",
                    "director's commentary",
                    "directors commentary",
                ],
            ),
            sdh: contains_token(
                &t,
                &[
                    "sdh",
                    "hearing impaired",
                    "hearing-impaired",
                    "cc",
                    "closed caption",
                    "closed captions",
                ],
            ),
            signs: contains_token(&t, &["signs", "sign", "signs & songs", "signs and songs"]),
            forced_title: contains_token(&t, &["forced"]),
        }
    }

    fn kind_penalty(self) -> u8 {
        // Normal dialogue = 0; demoted kinds share one band so language still
        // dominates and title tiebreak separates SDH from commentary.
        if self.commentary || self.sdh || self.signs {
            1
        } else {
            0
        }
    }
}

fn contains_token(hay: &str, needles: &[&str]) -> bool {
    for n in needles {
        if n.len() <= 2 {
            // Short tokens need word boundaries ("cc" must not match "codec").
            for part in hay.split(|c: char| !c.is_ascii_alphanumeric()) {
                if part == *n {
                    return true;
                }
            }
        } else if hay.contains(n) {
            return true;
        }
    }
    false
}

fn lang_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn track_lang_matches(track: &TrackCandidate, pref: &str) -> bool {
    track.language.as_deref().is_some_and(|l| lang_eq(l, pref))
}

fn is_forced(track: &TrackCandidate, bits: KindBits) -> bool {
    track.is_forced || bits.forced_title
}

fn audio_is_foreign(audio_language: Option<&str>, pref: &str) -> bool {
    match audio_language.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => !lang_eq(a, pref),
        None => false,
    }
}

/// Pick an audio track. When `preferred_language` is set and nothing matches,
/// falls back to the no-preference path with an honest reason prefix — audio
/// must map something. Stream order is only a last resort without a hit.
pub fn select_audio_track(
    tracks: &[TrackCandidate],
    preferred_language: Option<&str>,
) -> TrackSelection {
    if tracks.is_empty() {
        return TrackSelection {
            track_id: None,
            reason: "no audio tracks in file".into(),
        };
    }
    if let Some(pref) = preferred_language.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(sel) = best_audio_among(tracks, Some(pref)) {
            return sel;
        }
        let mut fallback = best_audio_among(tracks, None).unwrap_or(TrackSelection {
            track_id: None,
            reason: "no eligible audio track".into(),
        });
        fallback.reason = format!("no audio matched language {pref}; {}", fallback.reason);
        return fallback;
    }
    best_audio_among(tracks, None).unwrap_or(TrackSelection {
        track_id: None,
        reason: "no eligible audio track".into(),
    })
}

fn best_audio_among(tracks: &[TrackCandidate], pref: Option<&str>) -> Option<TrackSelection> {
    let mut best: Option<(u8, bool, bool, u32, usize)> = None;
    for (i, t) in tracks.iter().enumerate() {
        if let Some(p) = pref
            && !track_lang_matches(t, p)
        {
            continue;
        }
        let bits = KindBits::from_title(t.title.as_deref());
        // Prefer non-commentary; default flag; then lowest stream index.
        let key = (
            bits.kind_penalty(),
            !t.is_default,
            t.is_image,
            t.stream_index,
            i,
        );
        if best.as_ref().is_none_or(|b| key < *b) {
            best = Some(key);
        }
    }
    let (_, _, _, _, idx) = best?;
    let t = &tracks[idx];
    let lang = t.language.as_deref().unwrap_or("und");
    let reason = match pref {
        Some(p) if t.is_default => format!("{lang}, matched language {p}, container default"),
        Some(_) => format!("{lang}, matched your preference"),
        None if t.is_default => format!("{lang}, container default"),
        None => format!("{lang}, first eligible audio track in file"),
    };
    Some(TrackSelection {
        track_id: Some(t.track_id.clone()),
        reason,
    })
}

/// Pick a subtitle track, or none. Wrong-language is worse than silence.
pub fn select_subtitle_track(
    tracks: &[TrackCandidate],
    preferred_language: Option<&str>,
    audio_language: Option<&str>,
) -> TrackSelection {
    let Some(pref) = preferred_language.map(str::trim).filter(|s| !s.is_empty()) else {
        return TrackSelection {
            track_id: None,
            reason: "no subtitle preference; leaving subtitles off".into(),
        };
    };

    let foreign = audio_is_foreign(audio_language, pref);
    if foreign {
        return select_forced_subtitle(tracks, pref);
    }
    select_dialogue_subtitle(tracks, pref)
}

fn select_forced_subtitle(tracks: &[TrackCandidate], pref: &str) -> TrackSelection {
    let mut best: Option<(u8, bool, bool, u32, usize)> = None;
    for (i, t) in tracks.iter().enumerate() {
        if !track_lang_matches(t, pref) {
            continue;
        }
        let bits = KindBits::from_title(t.title.as_deref());
        if !is_forced(t, bits) {
            continue;
        }
        let key = (
            bits.kind_penalty(),
            !t.is_default,
            t.is_image,
            t.stream_index,
            i,
        );
        if best.as_ref().is_none_or(|b| key < *b) {
            best = Some(key);
        }
    }
    match best {
        Some((_, _, _, _, idx)) => {
            let t = &tracks[idx];
            let lang = t.language.as_deref().unwrap_or("und");
            TrackSelection {
                track_id: Some(t.track_id.clone()),
                reason: format!("{lang}, forced for foreign audio"),
            }
        }
        None => TrackSelection {
            track_id: None,
            reason: format!(
                "no forced track matched language {pref} for foreign audio; leaving subtitles off"
            ),
        },
    }
}

fn select_dialogue_subtitle(tracks: &[TrackCandidate], pref: &str) -> TrackSelection {
    let mut best: Option<(u8, bool, bool, u32, usize)> = None;
    for (i, t) in tracks.iter().enumerate() {
        if !track_lang_matches(t, pref) {
            continue;
        }
        let bits = KindBits::from_title(t.title.as_deref());
        // Matching audio: forced does not auto-select (Phase 2 item 5).
        if is_forced(t, bits) {
            continue;
        }
        let key = (
            bits.kind_penalty(),
            !t.is_default,
            t.is_image,
            t.stream_index,
            i,
        );
        if best.as_ref().is_none_or(|b| key < *b) {
            best = Some(key);
        }
    }
    match best {
        Some((_, _, _, _, idx)) => {
            let t = &tracks[idx];
            let bits = KindBits::from_title(t.title.as_deref());
            let lang = t.language.as_deref().unwrap_or("und");
            let reason = if bits.sdh {
                format!("{lang}, matched your preference (SDH)")
            } else {
                format!("{lang}, matched your preference")
            };
            TrackSelection {
                track_id: Some(t.track_id.clone()),
                reason,
            }
        }
        None => TrackSelection {
            track_id: None,
            reason: format!("no track matched language {pref}"),
        },
    }
}

/// Closed-list SDH detection for inventory DTOs (same tokens as the ranker).
pub fn title_looks_sdh(title: Option<&str>) -> bool {
    KindBits::from_title(title).sdh
}

/// Closed-list forced detection from title alone (disposition is separate).
pub fn title_looks_forced(title: Option<&str>) -> bool {
    KindBits::from_title(title).forced_title
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(
        id: &str,
        lang: &str,
        title: &str,
        stream_index: u32,
        is_forced: bool,
        is_default: bool,
    ) -> TrackCandidate {
        TrackCandidate {
            track_id: id.into(),
            language: Some(lang.into()),
            title: Some(title.into()),
            is_default,
            is_forced,
            is_image: false,
            stream_index,
        }
    }

    /// Heartstopper-class: Arabic first, English + SDH later.
    fn adv_subs() -> Vec<TrackCandidate> {
        vec![
            cand("e3", "ar", "Arabic", 3, false, false),
            cand("e4", "zh", "Chinese", 4, false, false),
            cand("e33", "en", "English", 33, false, false),
            cand("e34", "en", "English [SDH]", 34, false, false),
            cand("e26", "es", "Spanish", 26, false, false),
            cand("e27", "es", "European Spanish", 27, false, false),
        ]
    }

    fn adv_audio() -> Vec<TrackCandidate> {
        vec![
            cand("e1", "en", "Commentary", 1, false, false),
            cand("e2", "en", "Main", 2, false, false),
        ]
    }

    #[test]
    fn eng_preference_picks_english_not_arabic_or_sdh() {
        let sel = select_subtitle_track(&adv_subs(), Some("en"), Some("en"));
        assert_eq!(sel.track_id.as_deref(), Some("e33"), "{sel:?}");
        assert!(
            sel.reason.contains("matched your preference"),
            "{}",
            sel.reason
        );
        assert!(!sel.reason.to_ascii_lowercase().contains("sdh"));
    }

    #[test]
    fn unknown_preference_selects_nothing() {
        // Pref misses every language: foreign-audio path looks for forced, finds
        // none, leaves off (wrong-language dialogue must not win).
        let sel = select_subtitle_track(&adv_subs(), Some("xx"), Some("en"));
        assert_eq!(sel.track_id, None);
        assert!(
            sel.reason.contains("leaving subtitles off")
                || sel.reason.contains("no track matched language xx"),
            "{}",
            sel.reason
        );
    }

    #[test]
    fn audio_prefers_main_over_commentary() {
        let sel = select_audio_track(&adv_audio(), Some("en"));
        assert_eq!(sel.track_id.as_deref(), Some("e2"), "{sel:?}");
        assert!(
            sel.reason.contains("commentary") || sel.reason.contains("matched"),
            "{}",
            sel.reason
        );
    }

    #[test]
    fn foreign_audio_selects_forced_english() {
        let tracks = vec![
            cand("e2", "en", "English (Forced)", 2, true, false),
            cand("e3", "en", "English", 3, false, false),
        ];
        let sel = select_subtitle_track(&tracks, Some("en"), Some("ja"));
        assert_eq!(sel.track_id.as_deref(), Some("e2"), "{sel:?}");
        assert!(sel.reason.contains("forced"), "{}", sel.reason);
    }

    #[test]
    fn matching_audio_skips_forced_and_full_when_only_forced_matches_wrong() {
        let tracks = vec![
            cand("e2", "en", "English (Forced)", 2, true, false),
            cand("e3", "en", "English", 3, false, false),
        ];
        // Audio matches preference → forced must not auto-select; full English does.
        let sel = select_subtitle_track(&tracks, Some("en"), Some("en"));
        assert_eq!(sel.track_id.as_deref(), Some("e3"), "{sel:?}");
    }

    #[test]
    fn matching_ja_audio_selects_nothing_despite_english_tracks() {
        let tracks = vec![
            cand("e2", "en", "English (Forced)", 2, true, false),
            cand("e3", "en", "English", 3, false, false),
        ];
        let sel = select_subtitle_track(&tracks, Some("ja"), Some("ja"));
        assert_eq!(sel.track_id, None, "{sel:?}");
        assert!(
            sel.reason.contains("no track matched language ja"),
            "{}",
            sel.reason
        );
    }

    #[test]
    fn foreign_audio_without_forced_selects_nothing() {
        let tracks = vec![cand("e3", "en", "English", 3, false, false)];
        let sel = select_subtitle_track(&tracks, Some("en"), Some("ja"));
        assert_eq!(sel.track_id, None, "{sel:?}");
        assert!(
            sel.reason.contains("leaving subtitles off"),
            "{}",
            sel.reason
        );
    }

    #[test]
    fn first_stream_would_pick_commentary_rank_does_not() {
        // No default flags — Emby-style trap on the audio axis.
        let sel = select_audio_track(&adv_audio(), Some("en"));
        assert_ne!(sel.track_id.as_deref(), Some("e1"));
        assert_eq!(sel.track_id.as_deref(), Some("e2"));
    }
}
