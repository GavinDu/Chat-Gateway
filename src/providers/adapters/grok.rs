use async_trait::async_trait;
use crate::providers::traits::{ChatProvider, AdapterSelectors, ProviderRequest, ChunkStream, ProviderChunk};
use crate::config::ProviderConfig;
use crate::providers::adapters::common;
use std::time::Duration;

pub struct GrokAdapter {
    pub cfg: ProviderConfig,
}

#[async_trait]
impl ChatProvider for GrokAdapter {
    fn name(&self) -> &str { "grok" }
    fn aliases(&self) -> &[String] { &self.cfg.models }
    fn priority(&self) -> u32 { self.cfg.priority }

    async fn discover_models(&self, page: &chromiumoxide::Page) -> anyhow::Result<Vec<String>> {
        // Grok.com UI similar to ChatGPT. Also x.com/i/grok. Model picker shows Grok 3 vs Mini. Supports thinking traces.
        // URL: https://grok.com
        // Models hint: grok-3, grok-3-mini, grok-2
        page.goto("https://grok.com").await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;

        // Try to open model picker without sending message
        let picker_sels = ["button:has-text(\"Grok\")", "select.model-selector", "div.model-picker"];
        for sel in picker_sels {
            if let Ok(el) = page.find_element(sel).await {
                let _ = el.click().await;
                tokio::time::sleep(Duration::from_millis(600)).await;
                break;
            }
        }

        // Extract visible model labels
        let eval_js = r#"
            () => {
                const texts = [];
                document.querySelectorAll('[role="option"], [data-testid*="model"], button, li, div').forEach(el => {
                    const t = el.innerText?.trim();
                    if (t && t.length < 40 && /(grok|sonar|grok|mistral|codestral|claude|gpt|gemini|qwen|llama|pixtral)/i.test(t)) texts.push(t);
                });
                return [...new Set(texts)].slice(0, 20);
            }
        "#;
        let discovered = page.evaluate(eval_js).await
            .ok()
            .and_then(|v| v.into_value::<Vec<String>>().ok())
            .unwrap_or_default();

        if discovered.is_empty() {
            Ok(self.cfg.models.clone())
        } else {
            // Merge with configured aliases per Go behavior
            let mut merged = self.cfg.models.clone();
            for m in discovered {
                if !merged.iter().any(|x| x.eq_ignore_ascii_case(&m)) {
                    merged.push(m);
                }
            }
            Ok(merged)
        }
    }

    async fn send(&self, page: &chromiumoxide::Page, req: ProviderRequest) -> anyhow::Result<ChunkStream> {
        let prompt = req.messages.last().map(|m| m.content.clone()).unwrap_or_default();
        if prompt.is_empty() { anyhow::bail!("empty prompt"); }

        let input_sels = ["textarea[placeholder*=\"Ask\"]", "div[contenteditable=\"true\"][role=\"textbox\"]", "textarea[name=\"message\"]"];
        let send_sels = ["button[aria-label=\"Send\"]", "button[type=\"submit\"]:has(svg)", "button.send-button"];
        let response_sels = ["div[data-testid=\"grok-response\"]", "div.markdown", "div.response-content"];

        // 1. Wait for input ready
        common::wait_for_selector(page, &input_sels, 15000).await?;

        // 2. Focus + clear + type with stealth (Obscura already sets webdriver=undefined)
        let input_el = common::find_with_fallback(page, &input_sels).await?;
        common::human_type(&input_el, &prompt).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 3. Send
        let send_btn = common::find_with_fallback(page, &send_sels).await?;
        send_btn.click().await?;

        // 4. Stream response
        let page_clone = page.clone();
        let stream = async_stream::stream! {
            let mut last_len = 0usize;
            let mut last_full = String::new();
            let mut stable_ticks = 0;
            let mut ttft: Option<std::time::Instant> = None;
            let start = std::time::Instant::now();

            loop {
                if start.elapsed().as_secs() > 150 { 
                    yield Ok(ProviderChunk { content: String::new(), done: true });
                    break; 
                }

                tokio::time::sleep(Duration::from_millis(350)).await;

                let els = page_clone.find_elements(response_sels.last().copied().unwrap_or("div")).await;
                if let Ok(nodes) = els {
                    if let Some(last) = nodes.last() {
                        match last.inner_text().await {
                            Ok(Some(txt)) if !txt.is_empty() => {
                                if ttft.is_none() && !txt.is_empty() {
                                    ttft = Some(std::time::Instant::now());
                                    tracing::debug!(provider="grok", ttft_ms=%start.elapsed().as_millis(), "TTFT");
                                }
                                if txt.len() > last_len {
                                    let delta = txt[last_len..].to_string();
                                    last_len = txt.len();
                                    last_full = txt.clone();
                                    // Perplexity: strip citation markers like [1][2]
                                    let clean_delta = if "grok" == "perplexity" {
                                        regex::Regex::new(r"\[\d+\]").unwrap().replace_all(&delta, "").to_string()
                                    } else { delta };
                                    if !clean_delta.is_empty() {
                                        yield Ok(ProviderChunk { content: clean_delta, done: false });
                                        stable_ticks = 0;
                                    }
                                } else {
                                    stable_ticks += 1;
                                }

                                // Detect completion: no growth for ~2s + no streaming indicator
                                if stable_ticks > 6 {
                                    // Check for stop button / streaming attribute gone
                                    let still_streaming = page_clone.find_element(r#"[data-is-streaming="true"], button[aria-label*="Stop"], button:has-text("Stop")"#).await.is_ok();
                                    if !still_streaming {
                                        yield Ok(ProviderChunk { content: String::new(), done: true });
                                        break;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            // Final trace logging (mirrors Go performance trace: parse, memory, context, routing, browser acquisition, provider queue, page readiness, input, send, TTFT, generation)
            tracing::info!(provider="grok", total_ms=%start.elapsed().as_millis(), response_len=%last_full.len(), "generation complete");
        };

        Ok(Box::pin(stream))
    }

    fn selectors(&self) -> AdapterSelectors {
        AdapterSelectors {
            input_selector: "textarea[placeholder*=\"Ask\"]".to_string(),
            send_button: "button[aria-label=\"Send\"]".to_string(),
            response_container: "div[data-testid=\"grok-response\"]".to_string(),
            model_picker: "button:has-text(\"Grok\")".to_string(),
        }
    }
}
