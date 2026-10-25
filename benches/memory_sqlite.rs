#[path = "../src/memory.rs"]
mod memory;

use memory::{ChatMessage, UnifiedMemory};
use std::{
    env, fs,
    path::PathBuf,
    time::{Duration, Instant},
};

const ITERATIONS: usize = 500;

fn message(role: &str, content: impl Into<String>) -> ChatMessage {
    ChatMessage {
        role: role.to_owned(),
        content: content.into(),
    }
}

fn percentile(samples: &mut [Duration], percentile: usize) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len().saturating_sub(1)) * percentile).div_ceil(100);
    samples[index]
}

fn report(name: &str, mut samples: Vec<Duration>) {
    let total: Duration = samples.iter().copied().sum();
    let count = samples.len().max(1) as u32;
    let avg = total / count;
    let p50 = percentile(&mut samples, 50);
    let p95 = percentile(&mut samples, 95);
    let ops_per_second = samples.len() as f64 / total.as_secs_f64().max(f64::MIN_POSITIVE);
    println!(
        "{name:24} n={:<5} avg={:>9.2?} p50={:>9.2?} p95={:>9.2?} throughput={:>10.1} ops/s",
        samples.len(),
        avg,
        p50,
        p95,
        ops_per_second
    );
}

fn main() -> anyhow::Result<()> {
    // A unique temporary *.json override makes from_env choose a sibling SQLite
    // database and an absent legacy import file. Never touch repository data.
    let root = env::temp_dir().join(format!("chat-gateway-bench-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root)?;
    let legacy = root.join("legacy.json");
    let previous_override = env::var_os("CHAT_GATEWAY_MEMORY_PATH");
    env::set_var("CHAT_GATEWAY_MEMORY_PATH", &legacy);

    let result = (|| -> anyhow::Result<()> {
        let memory = UnifiedMemory::from_env(8192)?;
        let mut store_samples = Vec::with_capacity(ITERATIONS);
        for i in 0..ITERATIONS {
            let start = Instant::now();
            memory.store_turn(
                "store-bench".to_owned(),
                vec![message("user", format!("store prompt {i}"))],
                message("assistant", format!("store response {i}")),
                "benchmark".to_owned(),
                "bench-tab".to_owned(),
            )?;
            store_samples.push(start.elapsed());
        }
        report("sqlite store_turn", store_samples);

        let mut context_samples = Vec::with_capacity(ITERATIONS);
        for i in 0..ITERATIONS {
            let incoming = [message("user", format!("context prompt {i}"))];
            let start = Instant::now();
            let (prompt, messages) = memory.build_context(Some("store-bench"), &incoming).expect("benchmark context read");
            std::hint::black_box((prompt, messages));
            context_samples.push(start.elapsed());
        }
        report("build_context (500 turns)", context_samples);

        let mut affinity_samples = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            let start = Instant::now();
            std::hint::black_box(memory.get_provider("store-bench"));
            std::hint::black_box(memory.get_tab("store-bench"));
            affinity_samples.push(start.elapsed());
        }
        report("provider/tab lookup", affinity_samples);

        let mut delete_samples = Vec::with_capacity(ITERATIONS);
        for i in 0..ITERATIONS {
            let id = format!("delete-bench-{i}");
            memory.store_turn(
                id.clone(),
                vec![message("user", "delete me")],
                message("assistant", "stored"),
                "benchmark".to_owned(),
                "bench-tab".to_owned(),
            )?;
            let start = Instant::now();
            std::hint::black_box(memory.delete_conversation(&id)?);
            delete_samples.push(start.elapsed());
        }
        report("sqlite delete", delete_samples);
        Ok(())
    })();

    if let Some(previous) = previous_override {
        env::set_var("CHAT_GATEWAY_MEMORY_PATH", previous);
    } else {
        env::remove_var("CHAT_GATEWAY_MEMORY_PATH");
    }
    let cleanup = fs::remove_dir_all(&root);
    result?;
    cleanup?;
    Ok(())
}
