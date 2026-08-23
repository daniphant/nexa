//! Effort levels implied by a model id when the provider did not enumerate them.
//!
//! OpenCode keeps a family table for this (`openaiReasoningEfforts`): GPT-5.6+
//! includes `max`, GPT-5.2–5.5 include `xhigh` but not `max`, and unknown
//! ids fall through so the caller can use [`ReasoningEffort::COMMON`].

use nexa_protocol::ReasoningEffort;

const GPT5: &[ReasoningEffort] = &[
    ReasoningEffort::Minimal,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
const GPT51: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];
const GPT52: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];
/// GPT-5.6 Sol/Luna/Terra and later: OpenAI's documented set is
/// `none`/`low`/`medium`/`high`/`xhigh`/`max`. `none` is the picker's Default.
const GPT56: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];
const GROK45: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
];

/// Effort levels implied by `model_id` when listing metadata did not say.
///
/// Returns `None` for ids with no known family so callers can fall back to
/// [`ReasoningEffort::COMMON`].
#[must_use]
pub fn inferred_reasoning_efforts(model_id: &str) -> Option<&'static [ReasoningEffort]> {
    let id = model_leaf(model_id);
    if let Some(version) = gpt5_version(id) {
        return Some(if version >= 6 {
            GPT56
        } else if version >= 2 {
            GPT52
        } else {
            GPT51
        });
    }
    if is_gpt5_family(id) {
        return Some(GPT5);
    }
    if id.contains("grok-4.6") || id.contains("grok-4-6") {
        return Some(GPT52);
    }
    if id.contains("grok-4.5") || id.contains("grok-4-5") {
        return Some(GROK45);
    }
    None
}

fn model_leaf(model_id: &str) -> &str {
    model_id.rsplit('/').next().unwrap_or(model_id)
}

fn is_gpt5_family(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id == "gpt-5"
        || id.starts_with("gpt-5-")
        || id.starts_with("gpt-5.")
        || id.starts_with("gpt5-")
        || id.starts_with("gpt5.")
}

fn gpt5_version(id: &str) -> Option<u32> {
    let id = id.to_ascii_lowercase();
    let rest = id
        .strip_prefix("gpt-5")
        .or_else(|| id.strip_prefix("gpt5"))?;
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix('-'))?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt56_sol_and_luna_include_max() {
        for id in [
            "gpt-5.6-sol",
            "gpt-5.6-luna",
            "gpt-5.6-terra",
            "gpt-5.6",
            "openai/gpt-5.6-sol",
            "GPT-5.6-Sol",
        ] {
            let efforts = inferred_reasoning_efforts(id).unwrap();
            assert!(
                efforts.contains(&ReasoningEffort::Max),
                "{id} should include max"
            );
            assert!(efforts.contains(&ReasoningEffort::XHigh));
            assert!(!efforts.contains(&ReasoningEffort::Minimal));
        }
    }

    #[test]
    fn earlier_gpt5_versions_stop_before_max() {
        assert_eq!(inferred_reasoning_efforts("gpt-5.4"), Some(GPT52));
        assert!(
            !inferred_reasoning_efforts("gpt-5.4")
                .unwrap()
                .contains(&ReasoningEffort::Max)
        );
        assert_eq!(inferred_reasoning_efforts("gpt-5.1"), Some(GPT51));
        assert_eq!(inferred_reasoning_efforts("gpt-5"), Some(GPT5));
    }

    #[test]
    fn grok_46_matches_the_common_set_without_max() {
        let efforts = inferred_reasoning_efforts("grok-4.6").unwrap();
        assert_eq!(efforts, GPT52);
        assert!(!efforts.contains(&ReasoningEffort::Max));
    }

    #[test]
    fn unknown_ids_do_not_invent_a_family() {
        assert_eq!(inferred_reasoning_efforts("plain"), None);
        assert_eq!(inferred_reasoning_efforts("qwen3.8-max"), None);
        assert_eq!(inferred_reasoning_efforts("kimi-k3"), None);
    }
}
