use std::collections::HashMap;
use std::ffi::OsString;

use serde::Serialize;
use sysinfo::System;

#[derive(Clone, Serialize)]
pub struct AiWorkload {
    pub label: String,
    pub category: String,
    pub process_count: usize,
    pub total_cpu_percent: f32,
    pub total_memory_bytes: u64,
    pub example_command: String,
}

#[derive(Clone, Serialize, Default)]
pub struct AiSnapshot {
    pub workload_count: usize,
    pub total_cpu_percent: f32,
    pub total_memory_bytes: u64,
    pub top_workloads: Vec<AiWorkload>,
}

pub fn detect(process_name: &str, cmdline: &str) -> Option<(&'static str, &'static str)> {
    let haystack = format!(
        "{} {}",
        process_name.to_ascii_lowercase(),
        cmdline.to_ascii_lowercase()
    );
    const RULES: [(&str, &str, &str); 20] = [
        ("openclaw", "OpenClaw", "agent"),
        ("ollama", "Ollama", "model-runtime"),
        ("llama.cpp", "llama.cpp", "model-runtime"),
        ("llamacpp", "llama.cpp", "model-runtime"),
        ("vllm", "vLLM", "model-runtime"),
        ("lm studio", "LM Studio", "model-runtime"),
        ("lm-studio", "LM Studio", "model-runtime"),
        ("mlx", "MLX", "model-runtime"),
        ("open-webui", "Open WebUI", "ai-ui"),
        ("anythingllm", "AnythingLLM", "ai-ui"),
        ("comfyui", "ComfyUI", "image-pipeline"),
        ("automatic1111", "Automatic1111", "image-pipeline"),
        ("invokeai", "InvokeAI", "image-pipeline"),
        ("whisper", "Whisper", "speech"),
        ("cursor", "Cursor", "agent-tool"),
        ("cline", "Cline", "agent-tool"),
        ("aider", "Aider", "agent-tool"),
        ("codex", "Codex", "agent-tool"),
        ("continue", "Continue", "agent-tool"),
        ("claude", "Claude", "agent-tool"),
    ];

    for (needle, label, category) in RULES {
        if haystack.contains(needle) {
            return Some((label, category));
        }
    }
    None
}

pub fn os_strings_to_string(parts: &[OsString]) -> String {
    parts
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn build_snapshot(system: &System) -> AiSnapshot {
    let mut groups = HashMap::<String, AiWorkload>::new();

    for process in system.processes().values() {
        let command = os_strings_to_string(process.cmd());
        let name = process.name().to_string_lossy().into_owned();
        let Some((label, category)) = detect(&name, &command) else {
            continue;
        };

        let key = format!("{category}:{label}");
        let entry = groups.entry(key).or_insert_with(|| AiWorkload {
            label: label.to_owned(),
            category: category.to_owned(),
            process_count: 0,
            total_cpu_percent: 0.0,
            total_memory_bytes: 0,
            example_command: command.clone(),
        });
        entry.process_count += 1;
        entry.total_cpu_percent += process.cpu_usage();
        entry.total_memory_bytes += process.memory();
        if entry.example_command.is_empty() && !command.is_empty() {
            entry.example_command = command.clone();
        }
    }

    let mut top_workloads: Vec<_> = groups.into_values().collect();
    top_workloads.sort_by(|a, b| {
        b.total_cpu_percent
            .partial_cmp(&a.total_cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.total_memory_bytes.cmp(&a.total_memory_bytes))
    });
    let workload_count = top_workloads.len();
    let total_cpu_percent = top_workloads.iter().map(|w| w.total_cpu_percent).sum();
    let total_memory_bytes = top_workloads.iter().map(|w| w.total_memory_bytes).sum();
    top_workloads.truncate(6);

    AiSnapshot {
        workload_count,
        total_cpu_percent,
        total_memory_bytes,
        top_workloads,
    }
}
