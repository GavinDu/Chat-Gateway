pub mod qwen;
pub mod metaai;
pub mod claude;
pub mod chatgpt;
pub mod gemini;
pub mod grok;
pub mod mistral;
pub mod perplexity;
pub mod common;
pub mod diagnostics;

use crate::providers::traits::ChatProvider;
use std::sync::Arc;
use dashmap::DashMap;

pub fn build_registry(cfg: &crate::config::AppConfig) -> DashMap<String, Arc<dyn ChatProvider>> {
    let map = DashMap::new();
    for p in &cfg.providers {
        if !p.enabled { continue; }
        let provider: Arc<dyn ChatProvider> = match p.name.as_str() {
            "qwen" => Arc::new(qwen::QwenAdapter { cfg: p.clone() }),
            "metaai" => Arc::new(metaai::MetaAIAdapter { cfg: p.clone() }),
            "claude" => Arc::new(claude::ClaudeAdapter { cfg: p.clone() }),
            "chatgpt" => Arc::new(chatgpt::ChatGPTAdapter { cfg: p.clone() }),
            "gemini" => Arc::new(gemini::GeminiAdapter { cfg: p.clone() }),
            "grok" => Arc::new(grok::GrokAdapter { cfg: p.clone() }),
            "mistral" => Arc::new(mistral::MistralAdapter { cfg: p.clone() }),
            "perplexity" => Arc::new(perplexity::PerplexityAdapter { cfg: p.clone() }),
            _ => continue,
        };
        map.insert(p.name.clone(), provider);
    }
    map
}
