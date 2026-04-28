//! Static GLM model catalogue for the four Z.ai aliases.
//!
//! Mirrors `models.dev` as of 2026-04. Three catalogues exist because the four
//! aliases do **not** share pricing or model lists 1:1:
//!
//! - `zai-coding-plan` and `zhipuai-coding-plan` → `coding_plan()` (4 models,
//!   all costs zero — included in the flat-rate plan).
//! - `zai` → `zai_payg()` (13 models, public PAYG pricing).
//! - `zhipuai` → `zhipuai_payg()` (12 models; identical to `zai` except
//!   `glm-5.1` and `glm-5v-turbo` are ~4–5× more expensive on the China
//!   endpoint, and `glm-4.7-flashx` is absent).
//!
//! The catalogue is used only for metadata enrichment on `/v1/models` and to
//! drive `Capabilities::reasoning`-gated request shaping. Upstream `/models`
//! always wins on identity; entries unknown to this overlay still pass
//! through (and default to `reasoning=true` for any `glm-*` id, matching
//! Z.ai's own clients).

use crate::provider::{
    CacheCost, Capabilities, Cost, Interleaved, Limits, ModelInfo, Modalities,
};

/// Catalogue selector. The Z.ai provider picks one based on its alias id.
pub fn catalogue_for(alias: &str) -> Vec<ModelInfo> {
    match alias {
        crate::provider::ID_ZAI_CODING_PLAN | crate::provider::ID_ZHIPUAI_CODING_PLAN => {
            coding_plan()
        }
        crate::provider::ID_ZAI => zai_payg(),
        crate::provider::ID_ZHIPUAI => zhipuai_payg(),
        // Unknown alias — default to the union with PAYG pricing so callers
        // always get *some* metadata. Should be unreachable.
        _ => zai_payg(),
    }
}

/// Backwards-compat shim used by older callers (e.g. tests). Returns the
/// PAYG zai catalogue (the largest list).
#[allow(dead_code)]
pub fn catalogue() -> Vec<ModelInfo> { zai_payg() }

// ----- helpers --------------------------------------------------------------

fn caps_text(reasoning: bool, interleaved_field: bool) -> Capabilities {
    Capabilities {
        temperature: true,
        reasoning,
        attachment: false,
        toolcall: true,
        input: Modalities::TEXT_ONLY,
        output: Modalities::TEXT_ONLY,
        interleaved: if interleaved_field {
            Interleaved::Field { field: "reasoning_content".into() }
        } else {
            Interleaved::Disabled(false)
        },
    }
}

fn caps_vision(input: Modalities, interleaved_field: bool) -> Capabilities {
    Capabilities {
        temperature: true,
        reasoning: true,
        attachment: true,
        toolcall: true,
        input,
        output: Modalities::TEXT_ONLY,
        interleaved: if interleaved_field {
            Interleaved::Field { field: "reasoning_content".into() }
        } else {
            Interleaved::Disabled(false)
        },
    }
}

fn cost(input: f64, output: f64, cache_read: Option<f64>) -> Option<Cost> {
    Some(Cost {
        input,
        output,
        cache: cache_read.map(|r| CacheCost { read: r, write: 0.0 }),
    })
}

fn free() -> Option<Cost> {
    Some(Cost { input: 0.0, output: 0.0, cache: Some(CacheCost { read: 0.0, write: 0.0 }) })
}

fn vision_input_image_video() -> Modalities {
    Modalities { text: true, audio: false, image: true, video: true, pdf: false }
}

fn vision_input_full() -> Modalities {
    Modalities { text: true, audio: false, image: true, video: true, pdf: true }
}

// ----- catalogues -----------------------------------------------------------

fn coding_plan() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "glm-4.7".into(),
            name: "GLM-4.7".into(),
            capabilities: caps_text(true, true),
            cost: free(),
            limit: Limits { context: 204_800, output: 131_072 },
            release_date: Some("2025-12-22".into()),
        },
        ModelInfo {
            id: "glm-5.1".into(),
            name: "GLM-5.1".into(),
            capabilities: caps_text(true, true),
            cost: free(),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-03-27".into()),
        },
        ModelInfo {
            id: "glm-4.5-air".into(),
            name: "GLM-4.5-Air".into(),
            capabilities: caps_text(true, false),
            cost: free(),
            limit: Limits { context: 131_072, output: 98_304 },
            release_date: Some("2025-07-28".into()),
        },
        ModelInfo {
            id: "glm-5-turbo".into(),
            name: "GLM-5-Turbo".into(),
            capabilities: caps_text(true, true),
            cost: free(),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-03-16".into()),
        },
    ]
}

fn zai_payg() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "glm-5".into(),
            name: "GLM-5".into(),
            capabilities: caps_text(true, true),
            cost: cost(1.0, 3.2, Some(0.2)),
            limit: Limits { context: 204_800, output: 131_072 },
            release_date: Some("2026-02-11".into()),
        },
        ModelInfo {
            id: "glm-4.7".into(),
            name: "GLM-4.7".into(),
            capabilities: caps_text(true, true),
            cost: cost(0.6, 2.2, Some(0.11)),
            limit: Limits { context: 204_800, output: 131_072 },
            release_date: Some("2025-12-22".into()),
        },
        ModelInfo {
            id: "glm-4.7-flashx".into(),
            name: "GLM-4.7-FlashX".into(),
            capabilities: caps_text(true, false),
            cost: cost(0.07, 0.4, Some(0.01)),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-01-19".into()),
        },
        ModelInfo {
            id: "glm-4.6".into(),
            name: "GLM-4.6".into(),
            capabilities: caps_text(true, false),
            cost: cost(0.6, 2.2, Some(0.11)),
            limit: Limits { context: 204_800, output: 131_072 },
            release_date: Some("2025-09-30".into()),
        },
        ModelInfo {
            id: "glm-5.1".into(),
            name: "GLM-5.1".into(),
            capabilities: caps_text(true, true),
            cost: cost(1.4, 4.4, Some(0.26)),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-03-27".into()),
        },
        ModelInfo {
            id: "glm-4.5-flash".into(),
            name: "GLM-4.5-Flash".into(),
            capabilities: caps_text(true, false),
            cost: free(),
            limit: Limits { context: 131_072, output: 98_304 },
            release_date: Some("2025-07-28".into()),
        },
        ModelInfo {
            id: "glm-4.6v".into(),
            name: "GLM-4.6V".into(),
            capabilities: caps_vision(vision_input_image_video(), false),
            cost: cost(0.3, 0.9, None),
            limit: Limits { context: 128_000, output: 32_768 },
            release_date: Some("2025-12-08".into()),
        },
        ModelInfo {
            id: "glm-4.5-air".into(),
            name: "GLM-4.5-Air".into(),
            capabilities: caps_text(true, false),
            cost: cost(0.2, 1.1, Some(0.03)),
            limit: Limits { context: 131_072, output: 98_304 },
            release_date: Some("2025-07-28".into()),
        },
        ModelInfo {
            id: "glm-4.5v".into(),
            name: "GLM-4.5V".into(),
            capabilities: caps_vision(vision_input_image_video(), false),
            cost: cost(0.6, 1.8, None),
            limit: Limits { context: 64_000, output: 16_384 },
            release_date: Some("2025-08-11".into()),
        },
        ModelInfo {
            id: "glm-4.7-flash".into(),
            name: "GLM-4.7-Flash".into(),
            capabilities: caps_text(true, false),
            cost: free(),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-01-19".into()),
        },
        ModelInfo {
            id: "glm-5v-turbo".into(),
            name: "glm-5v-turbo".into(),
            capabilities: caps_vision(vision_input_full(), true),
            cost: cost(1.2, 4.0, Some(0.24)),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-04-01".into()),
        },
        ModelInfo {
            id: "glm-5-turbo".into(),
            name: "GLM-5-Turbo".into(),
            capabilities: caps_text(true, true),
            cost: cost(1.2, 4.0, Some(0.24)),
            limit: Limits { context: 200_000, output: 131_072 },
            release_date: Some("2026-03-16".into()),
        },
        ModelInfo {
            id: "glm-4.5".into(),
            name: "GLM-4.5".into(),
            capabilities: caps_text(true, false),
            cost: cost(0.6, 2.2, Some(0.11)),
            limit: Limits { context: 131_072, output: 98_304 },
            release_date: Some("2025-07-28".into()),
        },
    ]
}

fn zhipuai_payg() -> Vec<ModelInfo> {
    // Identical to zai_payg() except: glm-5.1 and glm-5v-turbo cost ~4-5x
    // more, and glm-4.7-flashx is absent.
    let mut out = zai_payg();
    out.retain(|m| m.id != "glm-4.7-flashx");
    for m in out.iter_mut() {
        match m.id.as_str() {
            "glm-5.1" => m.cost = cost(6.0, 24.0, Some(1.3)),
            "glm-5v-turbo" => m.cost = cost(5.0, 22.0, Some(1.2)),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ID_ZAI, ID_ZAI_CODING_PLAN, ID_ZHIPUAI, ID_ZHIPUAI_CODING_PLAN};

    #[test]
    fn coding_plan_is_free() {
        for m in coding_plan() {
            let c = m.cost.expect("coding plan models declare cost");
            assert_eq!(c.input, 0.0, "{} input not free", m.id);
            assert_eq!(c.output, 0.0, "{} output not free", m.id);
        }
    }

    #[test]
    fn zhipuai_marks_up_glm51_and_5v_turbo() {
        let z = zai_payg();
        let zh = zhipuai_payg();
        let z51 = z.iter().find(|m| m.id == "glm-5.1").unwrap().cost.as_ref().unwrap();
        let zh51 = zh.iter().find(|m| m.id == "glm-5.1").unwrap().cost.as_ref().unwrap();
        assert!(zh51.input > z51.input * 3.0, "zhipuai glm-5.1 should be markedly pricier");
        assert!(zh.iter().all(|m| m.id != "glm-4.7-flashx"), "flashx is zai-only");
    }

    #[test]
    fn alias_dispatch_picks_right_catalogue() {
        assert_eq!(catalogue_for(ID_ZAI_CODING_PLAN).len(), 4);
        assert_eq!(catalogue_for(ID_ZHIPUAI_CODING_PLAN).len(), 4);
        assert_eq!(catalogue_for(ID_ZAI).len(), 13);
        assert_eq!(catalogue_for(ID_ZHIPUAI).len(), 12);
    }

    #[test]
    fn all_glm_models_advertise_reasoning() {
        for m in zai_payg() {
            assert!(m.capabilities.reasoning, "{} should advertise reasoning", m.id);
        }
    }

    #[test]
    fn vision_models_have_attachment_and_image_input() {
        for id in ["glm-4.5v", "glm-4.6v", "glm-5v-turbo"] {
            let m = zai_payg().into_iter().find(|m| m.id == id).unwrap();
            assert!(m.capabilities.attachment, "{id} attachment");
            assert!(m.capabilities.input.image, "{id} image input");
        }
    }
}
