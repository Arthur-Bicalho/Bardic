//! Pure cue resolution. OWNER: resolver/engine agent.
//!
//! Rules (docs/design.md §1):
//! 1. Candidate cues come from levels in order Scene → Chapter → Campaign → Global,
//!    restricted to the levels present in `input.context`.
//! 2. Match passes, each pass walking all levels before the next pass starts:
//!    a. Exact: cue.situation_id == trigger.situation_id && cue.emotion_id == trigger.emotion_id
//!    (a `None` in the trigger must match `None` on the cue).
//!    b. If the trigger has BOTH tags: SituationOnly (cue situation matches, cue emotion None).
//!    c. If the trigger has BOTH tags: EmotionOnly (cue emotion matches, cue situation None).
//!    d. If the trigger has exactly ONE tag: Partial (cue has that tag and any value for the other).
//!    Within a level, lowest `position` then lowest `id` wins. Cues with no items are skipped.
//! 3. Ambience fallback: first default environment set among scene → chapter → campaign → global
//!    (`FallbackLevel` reports which). Tracks of that environment ordered by position; if the
//!    trigger has an emotion and some tracks carry that emotion, return only those, else all.
//!    Ambience options: repeat Context, shuffle true, volume None, default fades.
//!    If the environment has no tracks, try the next level's environment.
//! 4. Otherwise `Resolution::Nothing`.
//!
//! An empty trigger (no tags) skips step 2 and goes straight to ambience.
//!
//! Implementation notes:
//! - "Present in context" means: `CueOwner::Scene(id)` only counts when
//!   `context.scene_id == Some(id)`, likewise for chapter/campaign; `Global` always counts.
//!   Cues owned by any other entity are ignored even if they appear in `input.cues`.
//! - The same guard applies to the per-level default environments: `scene_default_env` is
//!   only consulted when `context.scene_id` is set, etc. (`global_default_env` always).
//! - Ambience tracks are ordered by `position`, then `id`.
use crate::model::{
    AmbienceTrack, CueOptions, CueOwner, FallbackLevel, MatchKind, PlayContext, RepeatMode,
    Resolution, ResolverInput, Trigger,
};

pub fn resolve(input: &ResolverInput, trigger: Trigger) -> Resolution {
    let has_tags = trigger.situation_id.is_some() || trigger.emotion_id.is_some();
    if has_tags {
        if let Some(found) = resolve_cue(input, trigger) {
            return found;
        }
    }
    resolve_ambience(input, trigger).unwrap_or(Resolution::Nothing)
}

/// 0 = Scene … 3 = Global; `None` when the owner is not part of the current context.
fn level_rank(owner: CueOwner, ctx: &PlayContext) -> Option<u8> {
    match owner {
        CueOwner::Scene(id) if ctx.scene_id == Some(id) => Some(0),
        CueOwner::Chapter(id) if ctx.chapter_id == Some(id) => Some(1),
        CueOwner::Campaign(id) if ctx.campaign_id == Some(id) => Some(2),
        CueOwner::Global => Some(3),
        _ => None,
    }
}

fn resolve_cue(input: &ResolverInput, t: Trigger) -> Option<Resolution> {
    let both = t.situation_id.is_some() && t.emotion_id.is_some();
    let mut passes = vec![MatchKind::Exact];
    if both {
        passes.push(MatchKind::SituationOnly);
        passes.push(MatchKind::EmotionOnly);
    } else if t.situation_id.is_some() || t.emotion_id.is_some() {
        passes.push(MatchKind::Partial);
    }
    for pass in passes {
        let best = input
            .cues
            .iter()
            .filter(|c| !c.items.is_empty())
            .filter(|c| match pass {
                MatchKind::Exact => {
                    c.situation_id == t.situation_id && c.emotion_id == t.emotion_id
                }
                MatchKind::SituationOnly => {
                    c.situation_id == t.situation_id && c.emotion_id.is_none()
                }
                MatchKind::EmotionOnly => c.emotion_id == t.emotion_id && c.situation_id.is_none(),
                MatchKind::Partial => match (t.situation_id, t.emotion_id) {
                    (Some(s), None) => c.situation_id == Some(s),
                    (None, Some(e)) => c.emotion_id == Some(e),
                    _ => false,
                },
            })
            .filter_map(|c| level_rank(c.owner, &input.context).map(|r| (r, c)))
            .min_by_key(|(rank, c)| (*rank, c.position, c.id));
        if let Some((_, cue)) = best {
            return Some(Resolution::Cue {
                cue: cue.clone(),
                matched: pass,
            });
        }
    }
    None
}

/// Playback options used for every ambience (endless shuffled loop).
pub fn ambience_options() -> CueOptions {
    CueOptions {
        repeat: RepeatMode::Context,
        shuffle: true,
        volume: None,
        ..CueOptions::default()
    }
}

fn resolve_ambience(input: &ResolverInput, t: Trigger) -> Option<Resolution> {
    let ctx = &input.context;
    let levels = [
        (
            ctx.scene_id.and(input.scene_default_env),
            FallbackLevel::Scene,
        ),
        (
            ctx.chapter_id.and(input.chapter_default_env),
            FallbackLevel::Chapter,
        ),
        (
            ctx.campaign_id.and(input.campaign_default_env),
            FallbackLevel::Campaign,
        ),
        (input.global_default_env, FallbackLevel::Global),
    ];
    for (env, from) in levels {
        let Some(env) = env else { continue };
        let mut tracks: Vec<&AmbienceTrack> = input
            .ambience
            .iter()
            .filter(|a| a.environment_id == env)
            .collect();
        if tracks.is_empty() {
            continue;
        }
        tracks.sort_by_key(|a| (a.position, a.id));
        if let Some(emotion) = t.emotion_id {
            if tracks.iter().any(|a| a.emotion_id == Some(emotion)) {
                tracks.retain(|a| a.emotion_id == Some(emotion));
            }
        }
        return Some(Resolution::Ambience {
            environment_id: env,
            from,
            tracks: tracks.into_iter().cloned().collect(),
            options: ambience_options(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cue, CueItem, Id, TrackRef};

    const COMBAT: Id = 1;
    const TAVERN: Id = 2;
    const DREAD: Id = 10;
    const TENSION: Id = 11;

    const CAMPAIGN: Id = 100;
    const CHAPTER: Id = 200;
    const SCENE: Id = 300;

    fn track(uri: &str) -> TrackRef {
        TrackRef {
            spotify_uri: uri.into(),
            title: uri.into(),
            subtitle: String::new(),
            art_url: None,
            duration_ms: None,
            start_offset_ms: 0,
        }
    }

    fn cue(id: Id, owner: CueOwner, sit: Option<Id>, emo: Option<Id>, position: i64) -> Cue {
        Cue {
            id,
            owner,
            name: format!("cue{id}"),
            situation_id: sit,
            emotion_id: emo,
            options: CueOptions::default(),
            position,
            items: vec![CueItem {
                id: id * 10,
                cue_id: id,
                track: track(&format!("spotify:track:{id}")),
                position: 0,
            }],
        }
    }

    fn amb(id: Id, env: Id, emo: Option<Id>, position: i64) -> AmbienceTrack {
        AmbienceTrack {
            id,
            environment_id: env,
            track: track(&format!("spotify:playlist:amb{id}")),
            emotion_id: emo,
            position,
        }
    }

    fn full_ctx() -> PlayContext {
        PlayContext {
            campaign_id: Some(CAMPAIGN),
            chapter_id: Some(CHAPTER),
            scene_id: Some(SCENE),
        }
    }

    fn input(cues: Vec<Cue>) -> ResolverInput {
        ResolverInput {
            context: full_ctx(),
            cues,
            ..Default::default()
        }
    }

    fn trig(s: Option<Id>, e: Option<Id>) -> Trigger {
        Trigger {
            situation_id: s,
            emotion_id: e,
        }
    }

    fn cue_id(r: &Resolution) -> (Id, MatchKind) {
        match r {
            Resolution::Cue { cue, matched } => (cue.id, *matched),
            other => panic!("expected cue, got {other:?}"),
        }
    }

    #[test]
    fn exact_scene_beats_chapter_campaign_global() {
        let cues = vec![
            cue(4, CueOwner::Global, Some(COMBAT), Some(DREAD), 0),
            cue(
                3,
                CueOwner::Campaign(CAMPAIGN),
                Some(COMBAT),
                Some(DREAD),
                0,
            ),
            cue(2, CueOwner::Chapter(CHAPTER), Some(COMBAT), Some(DREAD), 0),
            cue(1, CueOwner::Scene(SCENE), Some(COMBAT), Some(DREAD), 0),
        ];
        let t = trig(Some(COMBAT), Some(DREAD));
        assert_eq!(
            cue_id(&resolve(&input(cues.clone()), t)),
            (1, MatchKind::Exact)
        );
        assert_eq!(
            cue_id(&resolve(&input(cues[..3].to_vec()), t)),
            (2, MatchKind::Exact)
        );
        assert_eq!(
            cue_id(&resolve(&input(cues[..2].to_vec()), t)),
            (3, MatchKind::Exact)
        );
        assert_eq!(
            cue_id(&resolve(&input(cues[..1].to_vec()), t)),
            (4, MatchKind::Exact)
        );
    }

    #[test]
    fn exact_global_beats_situation_only_scene() {
        let cues = vec![
            cue(1, CueOwner::Scene(SCENE), Some(COMBAT), None, 0),
            cue(2, CueOwner::Scene(SCENE), None, Some(DREAD), 0),
            cue(3, CueOwner::Global, Some(COMBAT), Some(DREAD), 0),
        ];
        let r = resolve(&input(cues), trig(Some(COMBAT), Some(DREAD)));
        assert_eq!(cue_id(&r), (3, MatchKind::Exact));
    }

    #[test]
    fn situation_only_beats_emotion_only_across_levels() {
        let cues = vec![
            cue(1, CueOwner::Scene(SCENE), None, Some(DREAD), 0),
            cue(2, CueOwner::Global, Some(COMBAT), None, 0),
        ];
        let r = resolve(&input(cues), trig(Some(COMBAT), Some(DREAD)));
        assert_eq!(cue_id(&r), (2, MatchKind::SituationOnly));
    }

    #[test]
    fn situation_only_level_precedence() {
        let cues = vec![
            cue(1, CueOwner::Global, Some(COMBAT), None, 0),
            cue(2, CueOwner::Chapter(CHAPTER), Some(COMBAT), None, 0),
        ];
        let r = resolve(&input(cues), trig(Some(COMBAT), Some(DREAD)));
        assert_eq!(cue_id(&r), (2, MatchKind::SituationOnly));
    }

    #[test]
    fn emotion_only_used_last() {
        let cues = vec![
            cue(1, CueOwner::Global, None, Some(DREAD), 0),
            cue(2, CueOwner::Campaign(CAMPAIGN), None, Some(DREAD), 0),
            // Wrong situation, wrong emotion: never matches.
            cue(3, CueOwner::Scene(SCENE), Some(TAVERN), Some(TENSION), 0),
            // Situation matches but carries another emotion: not SituationOnly.
            cue(4, CueOwner::Scene(SCENE), Some(COMBAT), Some(TENSION), 0),
        ];
        let r = resolve(&input(cues), trig(Some(COMBAT), Some(DREAD)));
        assert_eq!(cue_id(&r), (2, MatchKind::EmotionOnly));
    }

    #[test]
    fn single_tag_trigger_prefers_exact_then_partial() {
        let cues = vec![
            cue(1, CueOwner::Scene(SCENE), Some(COMBAT), Some(DREAD), 0),
            cue(2, CueOwner::Global, Some(COMBAT), None, 0),
        ];
        // Situation-only trigger: an exact (no-emotion) cue anywhere beats a partial one.
        let r = resolve(&input(cues.clone()), trig(Some(COMBAT), None));
        assert_eq!(cue_id(&r), (2, MatchKind::Exact));
        // Emotion-only trigger: no exact cue, so the Combat+Dread cue matches partially.
        let r = resolve(&input(cues.clone()), trig(None, Some(DREAD)));
        assert_eq!(cue_id(&r), (1, MatchKind::Partial));
        // Without the exact cue, the situation trigger also falls back to the partial cue.
        let r = resolve(&input(vec![cues[0].clone()]), trig(Some(COMBAT), None));
        assert_eq!(cue_id(&r), (1, MatchKind::Partial));
    }

    #[test]
    fn both_tag_trigger_never_uses_partial() {
        // Combat+Tension must not pick a Combat+Dread cue.
        let cues = vec![cue(1, CueOwner::Scene(SCENE), Some(COMBAT), Some(DREAD), 0)];
        let r = resolve(&input(cues), trig(Some(COMBAT), Some(TENSION)));
        assert!(!matches!(r, Resolution::Cue { .. }));
    }

    #[test]
    fn position_then_id_tie_break_within_level() {
        let cues = vec![
            cue(5, CueOwner::Chapter(CHAPTER), Some(COMBAT), None, 2),
            cue(7, CueOwner::Chapter(CHAPTER), Some(COMBAT), None, 1),
            cue(6, CueOwner::Chapter(CHAPTER), Some(COMBAT), None, 1),
        ];
        let r = resolve(&input(cues), trig(Some(COMBAT), None));
        assert_eq!(cue_id(&r), (6, MatchKind::Exact));
    }

    #[test]
    fn level_beats_position() {
        let cues = vec![
            cue(1, CueOwner::Global, Some(COMBAT), None, -5),
            cue(2, CueOwner::Campaign(CAMPAIGN), Some(COMBAT), None, 99),
        ];
        let r = resolve(&input(cues), trig(Some(COMBAT), None));
        assert_eq!(cue_id(&r), (2, MatchKind::Exact));
    }

    #[test]
    fn empty_cues_are_skipped() {
        let mut empty = cue(1, CueOwner::Scene(SCENE), Some(COMBAT), None, 0);
        empty.items.clear();
        let cues = vec![empty, cue(2, CueOwner::Global, Some(COMBAT), None, 0)];
        let r = resolve(&input(cues), trig(Some(COMBAT), None));
        assert_eq!(cue_id(&r), (2, MatchKind::Exact));
    }

    #[test]
    fn levels_absent_from_context_are_ignored() {
        let cues = vec![
            cue(1, CueOwner::Scene(999), Some(COMBAT), None, 0),
            cue(2, CueOwner::Chapter(998), Some(COMBAT), None, 0),
            cue(3, CueOwner::Campaign(997), Some(COMBAT), None, 0),
            cue(4, CueOwner::Global, Some(COMBAT), None, 0),
        ];
        let r = resolve(&input(cues.clone()), trig(Some(COMBAT), None));
        assert_eq!(cue_id(&r), (4, MatchKind::Exact));

        // Context without a scene: scene cue ignored even if its id would otherwise match.
        let mut inp = input(vec![
            cue(1, CueOwner::Scene(SCENE), Some(COMBAT), None, 0),
            cue(2, CueOwner::Chapter(CHAPTER), Some(COMBAT), None, 0),
        ]);
        inp.context.scene_id = None;
        assert_eq!(
            cue_id(&resolve(&inp, trig(Some(COMBAT), None))),
            (2, MatchKind::Exact)
        );

        // No context at all: only Global counts.
        let mut inp = input(cues[..3].to_vec());
        inp.context = PlayContext::default();
        assert_eq!(resolve(&inp, trig(Some(COMBAT), None)), Resolution::Nothing);
    }

    #[test]
    fn ambience_fallback_chain() {
        let mut inp = input(vec![cue(1, CueOwner::Global, Some(TAVERN), None, 0)]);
        inp.scene_default_env = Some(50);
        inp.chapter_default_env = Some(51);
        inp.campaign_default_env = Some(52);
        inp.global_default_env = Some(53);
        inp.ambience = vec![
            amb(1, 51, None, 0),
            amb(2, 52, None, 0),
            amb(3, 53, None, 0),
        ];
        let t = trig(Some(COMBAT), None);

        // Scene env 50 has no tracks → chapter env 51.
        match resolve(&inp, t) {
            Resolution::Ambience {
                environment_id,
                from,
                tracks,
                options,
            } => {
                assert_eq!((environment_id, from), (51, FallbackLevel::Chapter));
                assert_eq!(tracks.iter().map(|a| a.id).collect::<Vec<_>>(), vec![1]);
                assert_eq!(options.repeat, RepeatMode::Context);
                assert!(options.shuffle);
                assert_eq!(options.volume, None);
                assert_eq!(options.fade_in_ms, CueOptions::default().fade_in_ms);
                assert_eq!(options.fade_out_ms, CueOptions::default().fade_out_ms);
            }
            other => panic!("{other:?}"),
        }

        inp.chapter_default_env = None;
        assert!(matches!(
            resolve(&inp, t),
            Resolution::Ambience {
                environment_id: 52,
                from: FallbackLevel::Campaign,
                ..
            }
        ));
        inp.campaign_default_env = None;
        assert!(matches!(
            resolve(&inp, t),
            Resolution::Ambience {
                environment_id: 53,
                from: FallbackLevel::Global,
                ..
            }
        ));
        inp.ambience.retain(|a| a.environment_id != 53);
        assert_eq!(resolve(&inp, t), Resolution::Nothing);
    }

    #[test]
    fn scene_env_wins_when_it_has_tracks() {
        let mut inp = input(vec![]);
        inp.scene_default_env = Some(50);
        inp.global_default_env = Some(53);
        inp.ambience = vec![amb(1, 53, None, 0), amb(2, 50, None, 0)];
        assert!(matches!(
            resolve(&inp, trig(Some(COMBAT), None)),
            Resolution::Ambience {
                environment_id: 50,
                from: FallbackLevel::Scene,
                ..
            }
        ));
    }

    #[test]
    fn default_envs_of_absent_levels_are_ignored() {
        let mut inp = input(vec![]);
        inp.context.scene_id = None;
        inp.scene_default_env = Some(50);
        inp.global_default_env = Some(53);
        inp.ambience = vec![amb(1, 53, None, 0), amb(2, 50, None, 0)];
        assert!(matches!(
            resolve(&inp, trig(Some(COMBAT), None)),
            Resolution::Ambience {
                environment_id: 53,
                from: FallbackLevel::Global,
                ..
            }
        ));
    }

    #[test]
    fn ambience_emotion_filter_and_order() {
        let mut inp = input(vec![]);
        inp.global_default_env = Some(53);
        inp.ambience = vec![
            amb(4, 53, Some(DREAD), 3),
            amb(1, 53, None, 0),
            amb(3, 53, Some(DREAD), 1),
            amb(2, 53, Some(TENSION), 1),
            amb(9, 99, Some(DREAD), 0), // other environment
        ];
        let ids = |r: Resolution| match r {
            Resolution::Ambience { tracks, .. } => tracks.iter().map(|a| a.id).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            ids(resolve(&inp, trig(Some(COMBAT), Some(DREAD)))),
            vec![3, 4]
        );
        // No track carries the emotion → all tracks, ordered by position then id.
        assert_eq!(ids(resolve(&inp, trig(None, Some(77)))), vec![1, 2, 3, 4]);
        // No emotion requested → all.
        assert_eq!(
            ids(resolve(&inp, trig(Some(COMBAT), None))),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn empty_trigger_goes_straight_to_ambience() {
        let mut inp = input(vec![cue(1, CueOwner::Scene(SCENE), None, None, 0)]);
        inp.chapter_default_env = Some(51);
        inp.ambience = vec![amb(1, 51, None, 0)];
        assert!(matches!(
            resolve(&inp, Trigger::default()),
            Resolution::Ambience {
                environment_id: 51,
                from: FallbackLevel::Chapter,
                ..
            }
        ));
        inp.ambience.clear();
        assert_eq!(resolve(&inp, Trigger::default()), Resolution::Nothing);
    }

    #[test]
    fn nothing_when_no_cue_and_no_ambience() {
        let inp = input(vec![cue(1, CueOwner::Global, Some(TAVERN), None, 0)]);
        assert_eq!(
            resolve(&inp, trig(Some(COMBAT), Some(DREAD))),
            Resolution::Nothing
        );
        assert_eq!(
            resolve(&ResolverInput::default(), trig(Some(COMBAT), None)),
            Resolution::Nothing
        );
    }
}
