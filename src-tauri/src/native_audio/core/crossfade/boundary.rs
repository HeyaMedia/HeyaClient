//! Smart crossfade driven by Heya's server-side structural boundary analysis.
//!
//! This intentionally mirrors the WebAudio `SmartCrossfade` strategy. A natural
//! fade uses a linear fade-out so we do not double-curve the source; an outro
//! without a detected fade uses the normal equal-power pair. Missing or unsafe
//! boundary hints return `None` so the caller can try MixRamp or timed fallback.

use super::curves::{
    generate_fade_in, generate_fade_out, generate_linear_fade_out, steps_for_duration,
};
use super::types::{CrossfadeParams, TransitionPlan};

const MIN_DURATION_SEC: f32 = 0.5;
const MIN_START_FRACTION: f32 = 0.6;

pub fn compute_boundary_transition(params: &CrossfadeParams) -> Option<TransitionPlan> {
    let track_end_ms = (params.out_duration_sec.max(0.0) * 1000.0).round() as u64;
    if track_end_ms == 0 {
        return None;
    }

    if params.out_fade_start_ms.is_none()
        && params.out_outro_start_ms.is_none()
        && params.out_silence_start_ms.is_none()
    {
        return None;
    }
    // WebAudio substitutes the track end when analysis found a fade/outro but
    // no earlier silence boundary. Preserve that exact fallback here.
    let silence_start_ms = params
        .out_silence_start_ms
        .unwrap_or(track_end_ms)
        .min(track_end_ms);
    let fade_start_ms = params.out_fade_start_ms.unwrap_or(0);
    let outro_start_ms = params.out_outro_start_ms.unwrap_or(0);
    let has_natural_fade = fade_start_ms > 0 && fade_start_ms < silence_start_ms;
    let start_ms = if has_natural_fade {
        fade_start_ms
    } else {
        outro_start_ms
    };
    let minimum_start_ms = (track_end_ms as f32 * MIN_START_FRACTION).round() as u64;

    if start_ms == 0 || start_ms >= silence_start_ms || start_ms < minimum_start_ms {
        return None;
    }

    let duration_sec = ((silence_start_ms - start_ms) as f32 / 1000.0).max(MIN_DURATION_SEC);
    let steps = steps_for_duration(duration_sec);
    Some(TransitionPlan {
        start_time_sec: start_ms as f32 / 1000.0,
        duration_sec,
        fade_out_curve: Some(if has_natural_fade {
            generate_linear_fade_out(steps, 1.0)
        } else {
            generate_fade_out(steps, 1.0)
        }),
        fade_in_curve: Some(generate_fade_in(steps, 1.0)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> CrossfadeParams {
        CrossfadeParams {
            out_duration_sec: 200.0,
            out_parent_key: "album:a".into(),
            in_parent_key: "album:b".into(),
            out_end_ramp: None,
            in_start_ramp: None,
            out_outro_start_ms: Some(190_000),
            out_fade_start_ms: Some(195_000),
            out_silence_start_ms: Some(199_000),
            crossfade_window_ms: 3_000,
            smart_crossfade_max_ms: 3_000,
            mixramp_db: -17.0,
            smart_crossfade_enabled: true,
            same_album_crossfade: false,
        }
    }

    #[test]
    fn natural_fade_matches_web_smart_transition() {
        let plan = compute_boundary_transition(&params()).unwrap();
        assert_eq!(plan.start_time_sec, 195.0);
        assert_eq!(plan.duration_sec, 4.0);
        let fade = plan.fade_out_curve.unwrap();
        assert!((fade[fade.len() / 2] - 0.5).abs() < 0.02);
    }

    #[test]
    fn outro_without_fade_uses_equal_power() {
        let mut input = params();
        input.out_fade_start_ms = None;
        let plan = compute_boundary_transition(&input).unwrap();
        assert_eq!(plan.start_time_sec, 190.0);
        assert_eq!(plan.duration_sec, 9.0);
        let fade = plan.fade_out_curve.unwrap();
        assert!(fade[fade.len() / 2] > 0.65);
    }

    #[test]
    fn early_or_missing_boundaries_fall_back() {
        let mut input = params();
        input.out_fade_start_ms = Some(20_000);
        input.out_outro_start_ms = Some(30_000);
        assert!(compute_boundary_transition(&input).is_none());
        input.out_silence_start_ms = None;
        assert!(compute_boundary_transition(&input).is_none());
    }

    #[test]
    fn missing_silence_uses_track_end_like_webaudio() {
        let mut input = params();
        input.out_silence_start_ms = None;
        let plan = compute_boundary_transition(&input).unwrap();
        assert_eq!(plan.start_time_sec, 195.0);
        assert_eq!(plan.duration_sec, 5.0);
    }
}
