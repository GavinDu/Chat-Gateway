use chromiumoxide::Page;
use std::time::Duration;

pub async fn find_with_fallback<'a>(page: &'a Page, selectors: &[&str]) -> anyhow::Result<chromiumoxide::Element> {
    for sel in selectors {
        if let Ok(el) = page.find_element(*sel).await {
            return Ok(el);
        }
    }
    anyhow::bail!("none of selectors matched: {:?}", selectors)
}

pub async fn find_all_with_fallback(page: &Page, selectors: &[&str]) -> anyhow::Result<Vec<chromiumoxide::Element>> {
    for sel in selectors {
        if let Ok(els) = page.find_elements(*sel).await {
            if !els.is_empty() { return Ok(els); }
        }
    }
    anyhow::bail!("no elements for {:?}", selectors)
}

pub async fn human_type(el: &chromiumoxide::Element, text: &str) -> anyhow::Result<()> {
    // Obscura has built-in stealth, but we add jitter
    el.click().await?;
    tokio::time::sleep(Duration::from_millis(80)).await;
    // Clear existing via select all + delete
    el.click().await?;
    let _ = el.type_str(text).await;
    Ok(())
}

pub async fn wait_for_selector(page: &Page, selectors: &[&str], timeout_ms: u64) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        for sel in selectors {
            if page.find_element(*sel).await.is_ok() { return Ok(()); }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    anyhow::bail!("timeout waiting for {:?}", selectors)
}
