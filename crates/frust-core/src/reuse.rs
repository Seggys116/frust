//! Whole-program LTO reuse. A hit requires every validated input to match.
//! Opaque proc-macro and build-script inputs are misses.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WholeLtoInputs {
    pub ordered_bitcode: Vec<[u8; 32]>,
    pub symbol_resolution: [u8; 32],
    pub roots: [u8; 32],
    pub target_options: [u8; 32],
    pub tool_version: [u8; 32],
    pub validated_extra: Vec<[u8; 32]>,
    pub opaque_proc_macro: bool,
    pub opaque_build_script: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReuseDecision {
    Hit,
    Miss { reason: &'static str },
}

pub fn decide_whole_lto(cached: Option<&WholeLtoInputs>, current: &WholeLtoInputs) -> ReuseDecision {
    if current.opaque_proc_macro {
        return ReuseDecision::Miss { reason: "opaque proc-macro input" };
    }
    if current.opaque_build_script {
        return ReuseDecision::Miss { reason: "opaque build-script input" };
    }
    let Some(cached) = cached else {
        return ReuseDecision::Miss { reason: "no cached whole-program result" };
    };
    if cached.opaque_proc_macro || cached.opaque_build_script {
        return ReuseDecision::Miss { reason: "cached whole-program input was opaque" };
    }
    if current.ordered_bitcode.is_empty() {
        return ReuseDecision::Miss { reason: "missing ordered bitcode" };
    }
    if cached.ordered_bitcode != current.ordered_bitcode {
        return ReuseDecision::Miss { reason: "ordered bitcode mismatch" };
    }
    if cached.symbol_resolution != current.symbol_resolution {
        return ReuseDecision::Miss { reason: "symbol resolution mismatch" };
    }
    if cached.roots != current.roots {
        return ReuseDecision::Miss { reason: "lto roots mismatch" };
    }
    if cached.target_options != current.target_options {
        return ReuseDecision::Miss { reason: "target options mismatch" };
    }
    if cached.tool_version != current.tool_version {
        return ReuseDecision::Miss { reason: "tool version mismatch" };
    }
    if cached.validated_extra != current.validated_extra {
        return ReuseDecision::Miss { reason: "whole-program input mismatch" };
    }
    ReuseDecision::Hit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WholeLtoInputs {
        WholeLtoInputs {
            ordered_bitcode: vec![[1; 32], [2; 32]],
            symbol_resolution: [3; 32],
            roots: [4; 32],
            target_options: [5; 32],
            tool_version: [6; 32],
            validated_extra: vec![[7; 32]],
            opaque_proc_macro: false,
            opaque_build_script: false,
        }
    }

    #[test]
    fn hit_requires_ordered_match_and_opaque_inputs_miss() {
        let current = sample();
        assert_eq!(decide_whole_lto(Some(&current), &current), ReuseDecision::Hit);
        let mut reordered = current.clone();
        reordered.ordered_bitcode.reverse();
        assert_eq!(
            decide_whole_lto(Some(&current), &reordered),
            ReuseDecision::Miss { reason: "ordered bitcode mismatch" }
        );
        let mut changed = current.clone();
        changed.validated_extra[0][0] = 9;
        assert_eq!(
            decide_whole_lto(Some(&current), &changed),
            ReuseDecision::Miss { reason: "whole-program input mismatch" }
        );
        let mut opaque = current.clone();
        opaque.opaque_proc_macro = true;
        assert_eq!(
            decide_whole_lto(Some(&current), &opaque),
            ReuseDecision::Miss { reason: "opaque proc-macro input" }
        );
        let mut script = current.clone();
        script.opaque_build_script = true;
        assert_eq!(
            decide_whole_lto(Some(&current), &script),
            ReuseDecision::Miss { reason: "opaque build-script input" }
        );
        assert_eq!(
            decide_whole_lto(None, &current),
            ReuseDecision::Miss { reason: "no cached whole-program result" }
        );
    }
}
