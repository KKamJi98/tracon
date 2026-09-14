use crate::model::{Observation, Source, State};

/// 같은 세션에 대한 여러 관측 중 화면에 쓸 하나를 고른다.
#[allow(dead_code)]
pub fn winner(observations: &[Observation]) -> Option<&Observation> {
    if observations.is_empty() {
        return None;
    }
    if let Some(dead) = observations
        .iter()
        .find(|o| o.source == Source::Layer0Inferred && o.state == State::Dead)
    {
        return Some(dead);
    }
    let latest_fact = observations
        .iter()
        .filter(|o| o.source != Source::Layer0Inferred)
        .max_by_key(|o| (o.observed_at, o.source));
    if latest_fact.is_some() {
        return latest_fact;
    }
    observations.iter().max_by_key(|o| o.observed_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    fn key() -> SessionKey {
        SessionKey {
            provider: Provider::Claude,
            uuid: "u1".into(),
        }
    }

    fn obs(state: State, source: Source, at: i64) -> Observation {
        let confidence = match source {
            Source::Layer0Inferred => Confidence::Medium,
            _ => Confidence::Fact,
        };
        Observation {
            key: key(),
            state,
            source,
            confidence,
            observed_at: at,
        }
    }

    #[test]
    fn fact_beats_inference() {
        let v = vec![
            obs(State::Unknown, Source::Layer0Inferred, 200),
            obs(State::WaitingApproval, Source::Layer1Hook, 100),
        ];
        assert_eq!(winner(&v).map(|o| o.state), Some(State::WaitingApproval));
    }

    #[test]
    fn layer0_dead_overrides_stale_fact() {
        let v = vec![
            obs(State::RunningTool, Source::Layer2Cmux, 100),
            obs(State::Dead, Source::Layer0Inferred, 300),
        ];
        assert_eq!(winner(&v).map(|o| o.state), Some(State::Dead));
    }

    #[test]
    fn newer_fact_wins_between_layers() {
        let v = vec![
            obs(State::RunningTool, Source::Layer2Cmux, 100),
            obs(State::WaitingInput, Source::Layer1Hook, 200),
        ];
        assert_eq!(winner(&v).map(|o| o.state), Some(State::WaitingInput));
    }

    #[test]
    fn cmux_wins_ties() {
        let v = vec![
            obs(State::WaitingInput, Source::Layer1Hook, 100),
            obs(State::RunningTool, Source::Layer2Cmux, 100),
        ];
        assert_eq!(winner(&v).map(|o| o.state), Some(State::RunningTool));
    }

    #[test]
    fn inference_only_is_used_when_no_facts() {
        let v = vec![obs(State::Unknown, Source::Layer0Inferred, 10)];
        assert_eq!(winner(&v).map(|o| o.state), Some(State::Unknown));
    }

    #[test]
    fn empty_input_has_no_winner() {
        assert!(winner(&[]).is_none());
    }
}
