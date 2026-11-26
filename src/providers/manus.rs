/// Pure task-correlated Agent lifecycle contract. It is compiled and
/// regression-tested, but the live DOM adapter must not consume task events
/// until first-party authenticated task-ID/evidence fixtures exist (#1041).
/// Suppress dormant public contract warnings only in this isolated module.
#[allow(dead_code)]
pub mod agent_lifecycle;

use crate::config::ProviderConfig;
use crate::providers::common::{self, SelectorSet};
use crate::providers::{AdapterSelectors, SiteAdapter};
use anyhow::Result;
use async_trait::async_trait;
use obscura_core::Page;

/// Manus web app. DOM selectors are conservative candidates until verified
/// against a signed-in account; no provider API key is required.
pub struct ManusAdapter {
    pub cfg: ProviderConfig,
}

pub const SELECTORS: SelectorSet = SelectorSet {
    url: "https://manus.im/app",
    picker: &[
        "button[aria-label*='model' i]",
        "[data-testid*='model-selector' i]",
    ],
    input: &[
        "textarea[placeholder*='Ask' i]",
        "textarea[placeholder*='message' i]",
        "[contenteditable='true'][role='textbox']",
        "div.ProseMirror[contenteditable='true']",
        "textarea",
    ],
    send: &[
        "button[aria-label='Send']",
        "button[aria-label*='Send' i]",
        "button[type='submit']",
    ],
    response: &[
        "[data-message-role='assistant']",
        "[data-role='assistant']",
        "[data-testid='assistant-message']",
        "[data-testid*='assistant-response' i]",
        "div.markdown-body",
        "div.prose",
    ],
    model_keywords: "manus|lite|max",
    model_pairs: &[],
};

#[async_trait(?Send)]
impl SiteAdapter for ManusAdapter {
    fn name(&self) -> &str {
        "manus"
    }

    fn aliases(&self) -> &[String] {
        &self.cfg.models
    }

    async fn discover_models(&self, page: &mut Page) -> Result<Vec<String>> {
        common::discover_models(page, &SELECTORS, &self.cfg.models, &self.cfg.selectors).await
    }

    async fn send_prompt(&self, page: &mut Page, prompt: &str) -> Result<()> {
        common::send_prompt(
            self.name(),
            page,
            &SELECTORS.effective(&self.cfg.selectors),
            prompt,
        )
        .await
    }

    fn response_text(&self, page: &Page) -> Option<String> {
        common::response_text(
            page,
            &SELECTORS.effective(&self.cfg.selectors).response_refs(),
        )
    }

    fn selectors(&self) -> AdapterSelectors {
        SELECTORS.effective(&self.cfg.selectors).adapter_selectors()
    }

    fn selector_set(&self) -> &common::SelectorSet {
        &SELECTORS
    }
    fn effective_selectors(&self) -> common::EffectiveSelectorSet {
        SELECTORS.effective(&self.cfg.selectors)
    }
}

#[cfg(test)]
mod agent_lifecycle_wiring_tests {
    use super::agent_lifecycle::{AgentEvent, AgentPhase, AgentTaskLifecycle, Transition};

    #[test]
    fn compiled_manus_agent_contract_never_treats_acceptance_as_completion() {
        let mut task = AgentTaskLifecycle::queued(11);
        let request = task.request_identity();
        assert_eq!(
            task.apply(&request, "opaque-current-task", AgentEvent::Accepted),
            Transition::Applied
        );
        assert_eq!(task.phase(), AgentPhase::Accepted);
        assert_eq!(
            task.apply(&request, "opaque-current-task", AgentEvent::Terminal),
            Transition::Invalid,
            "accepted task is not a completed Agent response"
        );
        assert_eq!(
            task.apply(&request, "different-task", AgentEvent::Running),
            Transition::Stale,
            "another task must not make the current request eligible to finish"
        );
        assert_eq!(
            task.apply(&request, "opaque-current-task", AgentEvent::Running),
            Transition::Applied
        );
        assert_eq!(
            task.apply(&request, "opaque-current-task", AgentEvent::Terminal),
            Transition::Applied
        );
        assert_eq!(task.phase(), AgentPhase::Terminal);
        assert_eq!(task.begin_new(12), Transition::Applied);
        assert_eq!(
            task.apply(&request, "opaque-current-task", AgentEvent::Terminal),
            Transition::Stale,
            "the previous owner's epoch cannot finish a new task"
        );
    }
}
