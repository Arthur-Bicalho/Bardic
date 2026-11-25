//! Pure cue resolution. OWNER: resolver/engine agent.
//!
//! Rules (docs/design.md §1):
//! 1. Candidate cues come from levels in order Scene → Chapter → Campaign → Global,
//!    restricted to the levels present in `input.context`.
//! 2. Match passes, each pass walking all levels before the next pass starts:
//!    a. Exact: cue.situation_id == trigger.situation_id && cue.emotion_id == trigger.emotion_id
//!       (a `None` in the trigger must match `None` on the cue).
//!    b. If the trigger has BOTH tags: SituationOnly (cue situation matches, cue emotion None).
//!    c. If the trigger has BOTH tags: EmotionOnly (cue emotion matches, cue situation None).
//!    Within a level, lowest `position` then lowest `id` wins. Cues with no items are skipped.
//! 3. Ambience fallback: first default environment set among scene → chapter → campaign → global
//!    (`FallbackLevel` reports which). Tracks of that environment ordered by position; if the
//!    trigger has an emotion and some tracks carry that emotion, return only those, else all.
//!    Ambience options: repeat Context, shuffle true, volume None, default fades.
//!    If the environment has no tracks, try the next level's environment.
//! 4. Otherwise `Resolution::Nothing`.
//! An empty trigger (no tags) skips step 2 and goes straight to ambience.
use crate::model::{Resolution, ResolverInput, Trigger};

pub fn resolve(_input: &ResolverInput, _trigger: Trigger) -> Resolution {
    todo!("resolver agent")
}
