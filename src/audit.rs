//! OpenCode transcript auditing and proxy pool capacity estimation.
//!
//! This module provides:
//! - Non-strict Serde deserialization for OpenCode transcript exports.
//! - Linear segmentation algorithm detecting rate-limit cutoff boundaries (`FreeUsageLimitError` / HTTP 429).
//! - Empirical quota analytics (output tokens, reasoning, cache read, duration per proxy).
//! - Proxy pool sizing estimation for projected workloads.

use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Nominal output token quota per IP address on OpenCode free models (e.g. `deepseek-v4.1-flash-free`).
pub const NOMINAL_OUTPUT_QUOTA_PER_IP: u64 = 60_000;

/// Root structure of an OpenCode transcript export.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionExport {
    pub info: Option<SessionInfo>,
    pub messages: Vec<SessionMessage>,
}

/// Metadata describing an OpenCode chat session.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionInfo {
    pub id: Option<String>,
    #[serde(rename = "projectID")]
    pub project_id: Option<String>,
    pub agent: Option<String>,
    pub model: Option<serde_json::Value>,
    pub cost: Option<f64>,
    pub tokens: Option<TokenUsage>,
    pub outcome: Option<String>,
    pub time: Option<SessionTime>,
    pub title: Option<String>,
}

impl SessionInfo {
    /// Extract the model identifier string whether represented as a string or a JSON object.
    pub fn model_name(&self) -> Option<String> {
        self.model.as_ref().and_then(extract_model_string)
    }
}

/// Helper to extract model name from either a string or `{"id": "...", ...}`.
fn extract_model_string(val: &serde_json::Value) -> Option<String> {
    if let Some(s) = val.as_str() {
        return Some(s.to_string());
    }
    if let Some(obj) = val.as_object() {
        if let Some(id) = obj.get("id").and_then(|v| v.as_str()) {
            return Some(id.to_string());
        }
    }
    None
}

/// Session-level timestamps.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionTime {
    pub created: Option<u64>,
    pub updated: Option<u64>,
    pub busy: Option<u64>,
    pub idle: Option<u64>,
}

/// A single message or event within an OpenCode conversation transcript.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionMessage {
    pub id: Option<String>,
    pub r#type: Option<String>,
    pub agent: Option<String>,
    pub model: Option<serde_json::Value>,
    pub time: Option<MessageTime>,
    pub tokens: Option<TokenUsage>,
    pub error: Option<SessionError>,
    pub finish: Option<String>,
    pub outcome: Option<String>,
}

/// Timestamps associated with a message lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MessageTime {
    pub created: Option<u64>,
    pub streamed: Option<u64>,
    pub completed: Option<u64>,
}

impl MessageTime {
    /// Return the earliest available timestamp in milliseconds.
    pub fn earliest(&self) -> Option<u64> {
        let mut times = Vec::with_capacity(3);
        if let Some(c) = self.created {
            times.push(c);
        }
        if let Some(s) = self.streamed {
            times.push(s);
        }
        if let Some(cp) = self.completed {
            times.push(cp);
        }
        times.into_iter().min()
    }

    /// Return the latest available timestamp in milliseconds.
    pub fn latest(&self) -> Option<u64> {
        let mut times = Vec::with_capacity(3);
        if let Some(c) = self.created {
            times.push(c);
        }
        if let Some(s) = self.streamed {
            times.push(s);
        }
        if let Some(cp) = self.completed {
            times.push(cp);
        }
        times.into_iter().max()
    }
}

/// Token metrics for input, output, reasoning, and context caching.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache: Option<CacheUsage>,
}

/// Cache token metrics.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CacheUsage {
    pub read: u64,
    pub write: u64,
}

/// Error details returned by OpenCode or upstream model providers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionError {
    pub status: Option<u16>,
    pub r#type: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "retryAfterMs")]
    pub retry_after_ms: Option<u64>,
    pub response: Option<SessionErrorResponse>,
}

/// Upstream response envelope attached to an error.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SessionErrorResponse {
    pub body: Option<String>,
}

/// Determines whether a message error represents a rate-limit or quota cutoff event.
pub fn is_rate_limit_error(err: &SessionError) -> bool {
    if err.status == Some(429) {
        return true;
    }

    if let Some(ref t) = err.r#type {
        let lower = t.to_lowercase();
        if lower.contains("free-limit")
            || lower.contains("rate-limit")
            || lower.contains("quota")
            || lower.contains("free_limit")
        {
            return true;
        }
    }

    if let Some(ref m) = err.message {
        let lower = m.to_lowercase();
        if lower.contains("free usage limit")
            || lower.contains("rate limit")
            || lower.contains("quota exceeded")
            || lower.contains("subscribe to opencode go")
            || lower.contains("free models work in opencode")
        {
            return true;
        }
    }

    if let Some(ref resp) = err.response {
        if let Some(ref b) = resp.body {
            let lower = b.to_lowercase();
            if lower.contains("freeusagelimiterror")
                || lower.contains("ratelimiterror")
                || lower.contains("quotaexceeded")
                || lower.contains("free usage limit")
            {
                return true;
            }
        }
    }

    false
}

/// Metrics aggregated for a single proxy consumption segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyCycle {
    pub cycle_index: usize,
    pub start_msg_idx: usize,
    pub end_msg_idx: usize,
    pub total_messages: usize,
    pub assistant_messages: usize,
    pub duration_seconds: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_generated_tokens: u64,
    pub total_billed_tokens: u64,
    pub is_active: bool,
    pub retry_after_ms: Option<u64>,
    pub error_message: Option<String>,
}

/// Comprehensive analysis of an OpenCode transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub session_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    pub total_messages: usize,
    pub total_cycles: usize,
    pub cycles: Vec<ProxyCycle>,
    pub average_cutoff_output: Option<u64>,
    pub min_cutoff_output: Option<u64>,
    pub max_cutoff_output: Option<u64>,
    pub active_cycle_remaining_tokens: Option<i64>,
    pub active_cycle_remaining_pct: Option<f64>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_reasoning_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub average_retry_after_ms: Option<u64>,
}

/// Parse an OpenCode transcript from a JSON string.
pub fn parse_transcript_from_str(json_str: &str) -> anyhow::Result<SessionExport> {
    let export: SessionExport = serde_json::from_str(json_str)?;
    Ok(export)
}

/// Parse an OpenCode transcript from a filesystem path.
pub fn parse_transcript_from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<SessionExport> {
    let path_ref = path.as_ref();
    let content = fs::read_to_string(path_ref).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read transcript file '{}': {}",
            path_ref.display(),
            e
        )
    })?;
    parse_transcript_from_str(&content)
}

/// Analyze a deserialized OpenCode transcript, segmenting message cycles by rate-limit rotations.
pub fn analyze_transcript(export: &SessionExport) -> AuditReport {
    let messages = &export.messages;
    let mut raw_cycles: Vec<(usize, usize, Vec<&SessionMessage>, Option<SessionError>)> =
        Vec::new();

    let mut current_cycle_msgs: Vec<(usize, &SessionMessage)> = Vec::new();

    for (idx, m) in messages.iter().enumerate() {
        let is_limit = m.error.as_ref().map(is_rate_limit_error).unwrap_or(false);

        if is_limit {
            if !current_cycle_msgs.is_empty() {
                let start_idx = current_cycle_msgs.first().map(|(i, _)| *i).unwrap_or(idx);
                let end_idx = current_cycle_msgs.last().map(|(i, _)| *i).unwrap_or(idx);
                let msgs_only: Vec<&SessionMessage> =
                    current_cycle_msgs.iter().map(|(_, msg)| *msg).collect();
                raw_cycles.push((start_idx, end_idx, msgs_only, m.error.clone()));
                current_cycle_msgs.clear();
            }
        } else {
            current_cycle_msgs.push((idx, m));
        }
    }

    if !current_cycle_msgs.is_empty() {
        let start_idx = current_cycle_msgs.first().map(|(i, _)| *i).unwrap_or(0);
        let end_idx = current_cycle_msgs.last().map(|(i, _)| *i).unwrap_or(0);
        let msgs_only: Vec<&SessionMessage> =
            current_cycle_msgs.iter().map(|(_, msg)| *msg).collect();
        raw_cycles.push((start_idx, end_idx, msgs_only, None));
    }

    let mut cycles = Vec::with_capacity(raw_cycles.len());
    let mut cutoff_outputs = Vec::new();
    let mut retry_after_values = Vec::new();

    let mut global_input = 0u64;
    let mut global_output = 0u64;
    let mut global_reasoning = 0u64;
    let mut global_cache_read = 0u64;

    let total_cycle_count = raw_cycles.len();

    for (cycle_num, (start_idx, end_idx, c_msgs, trigger_error)) in
        raw_cycles.into_iter().enumerate()
    {
        let is_active = trigger_error.is_none() && (cycle_num + 1 == total_cycle_count);
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut reasoning_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut assistant_count = 0usize;

        let mut t_start_ms: Option<u64> = None;
        let mut t_end_ms: Option<u64> = None;

        for m in &c_msgs {
            if m.r#type.as_deref() == Some("assistant") {
                assistant_count += 1;
            }

            if let Some(ref t) = m.time {
                if let Some(earliest) = t.earliest() {
                    t_start_ms = Some(t_start_ms.map_or(earliest, |prev| prev.min(earliest)));
                }
                if let Some(latest) = t.latest() {
                    t_end_ms = Some(t_end_ms.map_or(latest, |prev| prev.max(latest)));
                }
            }

            if let Some(ref tok) = m.tokens {
                input_tokens = input_tokens.saturating_add(tok.input);
                output_tokens = output_tokens.saturating_add(tok.output);
                reasoning_tokens = reasoning_tokens.saturating_add(tok.reasoning);
                if let Some(ref c) = tok.cache {
                    cache_read_tokens = cache_read_tokens.saturating_add(c.read);
                    cache_write_tokens = cache_write_tokens.saturating_add(c.write);
                }
            }
        }

        let duration_seconds = match (t_start_ms, t_end_ms) {
            (Some(s), Some(e)) if e >= s => (e - s) as f64 / 1000.0,
            _ => 0.0,
        };

        let retry_after_ms = trigger_error.as_ref().and_then(|e| e.retry_after_ms);
        if let Some(ra) = retry_after_ms {
            retry_after_values.push(ra);
        }

        let error_message = trigger_error.as_ref().and_then(|e| e.message.clone());

        if !is_active && trigger_error.is_some() {
            cutoff_outputs.push(output_tokens);
        }

        global_input = global_input.saturating_add(input_tokens);
        global_output = global_output.saturating_add(output_tokens);
        global_reasoning = global_reasoning.saturating_add(reasoning_tokens);
        global_cache_read = global_cache_read.saturating_add(cache_read_tokens);

        cycles.push(ProxyCycle {
            cycle_index: cycle_num + 1,
            start_msg_idx: start_idx,
            end_msg_idx: end_idx,
            total_messages: c_msgs.len(),
            assistant_messages: assistant_count,
            duration_seconds,
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cache_read_tokens,
            cache_write_tokens,
            total_generated_tokens: output_tokens.saturating_add(reasoning_tokens),
            total_billed_tokens: input_tokens.saturating_add(output_tokens),
            is_active,
            retry_after_ms,
            error_message,
        });
    }

    let average_cutoff_output = if !cutoff_outputs.is_empty() {
        let sum: u64 = cutoff_outputs.iter().sum();
        Some(sum / cutoff_outputs.len() as u64)
    } else {
        None
    };

    let min_cutoff_output = cutoff_outputs.iter().copied().min();
    let max_cutoff_output = cutoff_outputs.iter().copied().max();

    let average_retry_after_ms = if !retry_after_values.is_empty() {
        let sum: u64 = retry_after_values.iter().sum();
        Some(sum / retry_after_values.len() as u64)
    } else {
        None
    };

    let (active_cycle_remaining_tokens, active_cycle_remaining_pct) =
        if let Some(last_cycle) = cycles.iter().find(|c| c.is_active) {
            let quota = average_cutoff_output.unwrap_or(NOMINAL_OUTPUT_QUOTA_PER_IP);
            let used = last_cycle.output_tokens;
            let remaining = (quota as i64) - (used as i64);
            let pct = if quota > 0 {
                ((remaining as f64) / (quota as f64) * 100.0).max(0.0)
            } else {
                0.0
            };
            (Some(remaining), Some(pct))
        } else {
            (None, None)
        };

    let session_id = export.info.as_ref().and_then(|i| i.id.clone());
    let title = export.info.as_ref().and_then(|i| i.title.clone());
    let model = export.info.as_ref().and_then(|i| i.model_name());

    AuditReport {
        session_id,
        title,
        model,
        total_messages: messages.len(),
        total_cycles: cycles.len(),
        cycles,
        average_cutoff_output,
        min_cutoff_output,
        max_cutoff_output,
        active_cycle_remaining_tokens,
        active_cycle_remaining_pct,
        total_input_tokens: global_input,
        total_output_tokens: global_output,
        total_reasoning_tokens: global_reasoning,
        total_cache_read_tokens: global_cache_read,
        average_retry_after_ms,
    }
}

/// Sizing recommendation for an OpenCode proxy pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolEstimation {
    pub tokens_out: u64,
    pub tokens_in: Option<u64>,
    pub quota_per_proxy: u64,
    pub min_proxies: usize,
    pub recommended_proxies: usize,
    pub estimated_cooldown_hours: f64,
}

/// Estimate the required proxy pool dimension based on projected output workload.
pub fn estimate_pool_size(
    tokens_out: u64,
    tokens_in: Option<u64>,
    quota_per_proxy: Option<u64>,
) -> PoolEstimation {
    let quota = quota_per_proxy
        .unwrap_or(NOMINAL_OUTPUT_QUOTA_PER_IP)
        .max(1);
    let min_proxies = if tokens_out == 0 {
        1
    } else {
        tokens_out.div_ceil(quota) as usize
    };
    let min_proxies = min_proxies.max(1);
    let recommended_proxies = min_proxies + 1; // Operational buffer

    PoolEstimation {
        tokens_out,
        tokens_in,
        quota_per_proxy: quota,
        min_proxies,
        recommended_proxies,
        estimated_cooldown_hours: 8.8,
    }
}

/// Format integer with comma thousands separators.
pub fn format_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();

    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Format duration in seconds into human-readable string (minutes or seconds).
pub fn format_duration(seconds: f64) -> String {
    if seconds >= 60.0 {
        format!("{:.1} min", seconds / 60.0)
    } else {
        format!("{:.0} s", seconds)
    }
}

/// Print formatted table and empirical analysis for an audit report.
pub fn print_audit_report(report: &AuditReport) {
    println!(
        "{}",
        "========================================================================================="
            .cyan()
    );
    println!(
        "{} {}",
        "OPLIRÉ FORENSIC AUDIT — OpenCode Quota & Rotation Analysis"
            .bold()
            .cyan(),
        format!("(v{})", env!("CARGO_PKG_VERSION")).dimmed()
    );
    println!(
        "{}",
        "========================================================================================="
            .cyan()
    );

    if let Some(ref title) = report.title {
        println!("  {} {}", "Session Title:".bold(), title.white());
    }
    if let Some(ref sid) = report.session_id {
        println!("  {} {}", "Session ID   :".bold(), sid.dimmed());
    }
    if let Some(ref model) = report.model {
        println!("  {} {}", "Model Target :".bold(), model.green());
    }
    println!(
        "  {} {} messages across {} proxy cycles",
        "Volume       :".bold(),
        report.total_messages,
        report.total_cycles
    );
    println!();

    // Table Header
    println!(
        "{:<11} {:<15} {:<10} {:>11} {:>11} {:>11} {:>13}  {:<20}",
        "Proxy".bold(),
        "Msgs (Asst)".bold(),
        "Duration".bold(),
        "Input Tok".bold(),
        "Output Tok".bold(),
        "Reason Tok".bold(),
        "Cache Read".bold(),
        "Status / Rotation Trigger".bold()
    );
    println!("{}", "-".repeat(105).dimmed());

    for cycle in &report.cycles {
        let proxy_raw = if cycle.is_active {
            format!("Proxy {} ★", cycle.cycle_index)
        } else {
            format!("Proxy {}", cycle.cycle_index)
        };
        let proxy_col = if cycle.is_active {
            format!("{:<11}", proxy_raw).green().bold().to_string()
        } else {
            format!("{:<11}", proxy_raw).bold().to_string()
        };

        let msgs_raw = format!(
            "{} ({} asst)",
            cycle.total_messages, cycle.assistant_messages
        );
        let msgs_col = format!("{:<15}", msgs_raw);

        let duration_raw = format_duration(cycle.duration_seconds);
        let duration_col = format!("{:<10}", duration_raw);

        let in_col = format!("{:>11}", format_tokens(cycle.input_tokens));
        let out_raw = format!("{:>11}", format_tokens(cycle.output_tokens));
        let out_col = if cycle.is_active {
            out_raw.green().bold().to_string()
        } else {
            out_raw.bold().to_string()
        };
        let reas_col = format!("{:>11}", format_tokens(cycle.reasoning_tokens))
            .dimmed()
            .to_string();
        let cache_col = format!("{:>13}", format_tokens(cycle.cache_read_tokens))
            .dimmed()
            .to_string();

        let status_col = if cycle.is_active {
            if let Some(pct) = report.active_cycle_remaining_pct {
                format!("Active (~{:.1}% quota left)", pct)
                    .green()
                    .to_string()
            } else {
                "Active (In Use)".green().to_string()
            }
        } else if let Some(ms) = cycle.retry_after_ms {
            let hours = ms as f64 / 3_600_000.0;
            format!("429 Limit (~{:.2}h retry)", hours)
                .red()
                .to_string()
        } else {
            "Rotated (Cutoff)".yellow().to_string()
        };

        println!(
            "{} {} {} {} {} {} {}  {}",
            proxy_col, msgs_col, duration_col, in_col, out_col, reas_col, cache_col, status_col
        );
    }

    println!("{}", "-".repeat(105).dimmed());

    // Totals row
    println!(
        "{:<10} {:<15} {:<10} {:>11} {:>11} {:>11} {:>13}",
        "TOTAL".bold().cyan(),
        format!("{} msgs", report.total_messages).dimmed(),
        "".dimmed(),
        format_tokens(report.total_input_tokens).bold(),
        format_tokens(report.total_output_tokens).bold().cyan(),
        format_tokens(report.total_reasoning_tokens).dimmed(),
        format_tokens(report.total_cache_read_tokens).dimmed()
    );
    println!();

    // Empirical findings section
    println!("{}", "Empirical Quota & Capacity Findings:".bold().yellow());
    if let Some(avg_cut) = report.average_cutoff_output {
        let min_cut = report.min_cutoff_output.unwrap_or(avg_cut);
        let max_cut = report.max_cutoff_output.unwrap_or(avg_cut);
        println!(
            "  {} ~{} tokens (range: {} - {})",
            "• Measured Output Cutoff Threshold:".bold(),
            format_tokens(avg_cut).bold().green(),
            format_tokens(min_cut),
            format_tokens(max_cut)
        );
        println!(
            "  {} The quota trigger is strictly driven by {} (~60k-65k tokens/IP).",
            "• Rate-Limit Metric Driver       :".bold(),
            "Output Completion Tokens".bold().underline()
        );
    } else {
        println!(
            "  {} Nominal threshold is ~{} output tokens / IP.",
            "• Cutoff Threshold:".bold(),
            format_tokens(NOMINAL_OUTPUT_QUOTA_PER_IP).green()
        );
    }

    if let Some(avg_ra) = report.average_retry_after_ms {
        let hours = avg_ra as f64 / 3_600_000.0;
        println!(
            "  {} ~{:.2} hours ({} ms sliding window)",
            "• Measured Recharge Window       :".bold(),
            hours,
            format_tokens(avg_ra)
        );
    }

    if let (Some(rem_tok), Some(pct)) = (
        report.active_cycle_remaining_tokens,
        report.active_cycle_remaining_pct,
    ) {
        let formatted_rem = if rem_tok >= 0 {
            format!("+{} tokens available", format_tokens(rem_tok as u64)).green()
        } else {
            format!(
                "{} tokens over nominal quota",
                format_tokens((-rem_tok) as u64)
            )
            .red()
        };
        println!(
            "  {} {} ({:.1}% capacity remaining)",
            "• Current Active Proxy Status     :".bold(),
            formatted_rem,
            pct
        );
    }

    println!();
    println!("{}", "Sizing Rule (Per-IP Capacity):".bold().cyan());
    println!("  Formula: N_proxies = ceil(Target_Output_Tokens / 60,000) + 1 (buffer)");
    println!("  Recharge: ~8 to 9 hours before exhausted residential IPs return to service.");
    println!(
        "{}",
        "========================================================================================="
            .cyan()
    );
}

/// Print formatted pool estimation recommendation.
pub fn print_pool_estimation(est: &PoolEstimation) {
    println!(
        "{}",
        "===================================================================".cyan()
    );
    println!(
        "{} {}",
        "OPLIRÉ PROXY POOL SIZING ESTIMATOR".bold().cyan(),
        format!("(v{})", env!("CARGO_PKG_VERSION")).dimmed()
    );
    println!(
        "{}",
        "===================================================================".cyan()
    );
    println!();

    println!("{}", "Projected Workload:".bold().yellow());
    println!(
        "  {} {} tokens",
        "• Target Output Tokens :".bold(),
        format_tokens(est.tokens_out).green().bold()
    );
    if let Some(inp) = est.tokens_in {
        let ratio = if est.tokens_out > 0 {
            inp as f64 / est.tokens_out as f64
        } else {
            0.0
        };
        println!(
            "  {} {} tokens (Input/Output ratio: {:.2}x)",
            "• Target Input Tokens  :".bold(),
            format_tokens(inp).dimmed(),
            ratio
        );
        println!(
            "  {} {} tokens",
            "• Total Billed Tokens  :".bold(),
            format_tokens(inp.saturating_add(est.tokens_out)).bold()
        );
    }
    println!(
        "  {} {} output tokens / IP",
        "• Quota per Proxy IP   :".bold(),
        format_tokens(est.quota_per_proxy).dimmed()
    );
    println!();

    println!("{}", "Sizing Recommendation:".bold().yellow());
    println!(
        "  {} {} proxies (strict mathematical minimum)",
        "• Minimum Pool Size    :".bold(),
        est.min_proxies.to_string().bold()
    );
    println!(
        "  {} {} proxies (includes +1 operational safety reserve)",
        "• Recommended Pool Size:".bold(),
        est.recommended_proxies.to_string().bold().green()
    );
    println!(
        "  {} ~{:.1} hours (sliding cooldown before IP reuse)",
        "• Quota Recharge Window:".bold(),
        est.estimated_cooldown_hours
    );
    println!();

    println!("{}", "Deployment Steps:".bold().cyan());
    println!(
        "  1. Add {} Static Residential/ISP proxies to ~/.config/oplire/proxies.txt",
        est.recommended_proxies
    );
    println!("     oplire proxies import <path-to-proxies.txt>");
    println!("  2. Start the local CONNECT proxy server with automatic rate-limit rotation:");
    println!("     oplire proxy");
    println!("  3. Export OpenCode proxy routing variables in your shell:");
    println!("     eval $(oplire env)");
    println!(
        "{}",
        "===================================================================".cyan()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1,000");
        assert_eq!(format_tokens(61281), "61,281");
        assert_eq!(format_tokens(116584704), "116,584,704");
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(45.0), "45 s");
        assert_eq!(format_duration(60.0), "1.0 min");
        assert_eq!(format_duration(2851.0), "47.5 min");
    }

    #[test]
    fn test_is_rate_limit_error_detection() {
        let err_429 = SessionError {
            status: Some(429),
            ..Default::default()
        };
        assert!(is_rate_limit_error(&err_429));

        let err_free_limit = SessionError {
            status: Some(200),
            r#type: Some("provider.free-limit".to_string()),
            ..Default::default()
        };
        assert!(is_rate_limit_error(&err_free_limit));

        let err_message = SessionError {
            message: Some("Free usage limit reached. Connect a provider".to_string()),
            ..Default::default()
        };
        assert!(is_rate_limit_error(&err_message));

        let err_body = SessionError {
            response: Some(SessionErrorResponse {
                body: Some("{\"type\":\"FreeUsageLimitError\"}".to_string()),
            }),
            ..Default::default()
        };
        assert!(is_rate_limit_error(&err_body));

        let err_transport = SessionError {
            status: Some(200),
            r#type: Some("provider.transport".to_string()),
            message: Some("Connection lost: ECONNRESET".to_string()),
            ..Default::default()
        };
        assert!(!is_rate_limit_error(&err_transport));
    }

    #[test]
    fn test_estimate_pool_size() {
        let est0 = estimate_pool_size(0, None, None);
        assert_eq!(est0.min_proxies, 1);
        assert_eq!(est0.recommended_proxies, 2);

        let est_exact = estimate_pool_size(60_000, None, None);
        assert_eq!(est_exact.min_proxies, 1);
        assert_eq!(est_exact.recommended_proxies, 2);

        let est_plus_one = estimate_pool_size(60_001, None, None);
        assert_eq!(est_plus_one.min_proxies, 2);
        assert_eq!(est_plus_one.recommended_proxies, 3);

        let est_150k = estimate_pool_size(150_000, Some(500_000), None);
        assert_eq!(est_150k.min_proxies, 3);
        assert_eq!(est_150k.recommended_proxies, 4);
    }

    #[test]
    fn test_synthetic_segmentation() {
        let json = r#"{
            "info": {
                "id": "ses_test_123",
                "title": "Synthetic Test",
                "model": {"id": "deepseek-v4.1-flash-free"}
            },
            "messages": [
                {
                    "type": "assistant",
                    "time": {"created": 1000, "completed": 2000},
                    "tokens": {"input": 1000, "output": 30000, "reasoning": 5000, "cache": {"read": 100000, "write": 0}}
                },
                {
                    "type": "assistant",
                    "time": {"created": 2000, "completed": 3000},
                    "tokens": {"input": 1000, "output": 31000, "reasoning": 5000, "cache": {"read": 100000, "write": 0}}
                },
                {
                    "type": "assistant",
                    "error": {
                        "status": 429,
                        "type": "provider.free-limit",
                        "retryAfterMs": 32400000
                    }
                },
                {
                    "type": "idle",
                    "error": {
                        "status": 429,
                        "type": "provider.free-limit",
                        "retryAfterMs": 32400000
                    }
                },
                {
                    "type": "user",
                    "time": {"created": 3500}
                },
                {
                    "type": "assistant",
                    "time": {"created": 4000, "completed": 5000},
                    "tokens": {"input": 500, "output": 12000, "reasoning": 2000, "cache": {"read": 50000, "write": 0}}
                }
            ]
        }"#;

        let export = parse_transcript_from_str(json).expect("valid synthetic json");
        let report = analyze_transcript(&export);

        assert_eq!(report.total_messages, 6);
        assert_eq!(report.total_cycles, 2);

        // Cycle 1
        assert_eq!(cycle_index(&report.cycles, 1).output_tokens, 61000);
        assert!(!cycle_index(&report.cycles, 1).is_active);
        assert_eq!(
            cycle_index(&report.cycles, 1).retry_after_ms,
            Some(32400000)
        );

        // Cycle 2
        assert_eq!(cycle_index(&report.cycles, 2).output_tokens, 12000);
        assert!(cycle_index(&report.cycles, 2).is_active);

        // Stats
        assert_eq!(report.average_cutoff_output, Some(61000));
        assert_eq!(report.min_cutoff_output, Some(61000));
        assert_eq!(report.max_cutoff_output, Some(61000));
        assert_eq!(report.active_cycle_remaining_tokens, Some(49000));
    }

    fn cycle_index(cycles: &[ProxyCycle], idx: usize) -> &ProxyCycle {
        cycles
            .iter()
            .find(|c| c.cycle_index == idx)
            .expect("cycle exists")
    }
}
