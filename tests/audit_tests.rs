//! Comprehensive tests for oplire transcript auditing and pool estimation.

use oplire_reset::audit::{
    analyze_transcript, estimate_pool_size, is_rate_limit_error, parse_transcript_from_file,
    parse_transcript_from_str, SessionError, SessionErrorResponse, NOMINAL_OUTPUT_QUOTA_PER_IP,
};
use std::path::Path;

#[test]
fn test_synthetic_audit_with_rate_limit_rotation() {
    let json_data = r#"{
        "info": {
            "id": "ses_synth_001",
            "projectID": "proj_abc123",
            "agent": "build",
            "model": {
                "id": "deepseek-v4.1-flash-free",
                "providerID": "opencode",
                "variant": "max"
            },
            "title": "Synthetic Audit Test"
        },
        "messages": [
            {
                "id": "m1",
                "type": "assistant",
                "time": {"created": 1000, "completed": 2000},
                "tokens": {"input": 1500, "output": 35000, "reasoning": 10000, "cache": {"read": 500000, "write": 0}}
            },
            {
                "id": "m2",
                "type": "assistant",
                "time": {"created": 2000, "completed": 3500},
                "tokens": {"input": 2000, "output": 26000, "reasoning": 12000, "cache": {"read": 600000, "write": 0}}
            },
            {
                "id": "m3",
                "type": "assistant",
                "time": {"created": 3600, "completed": 3700},
                "finish": "error",
                "error": {
                    "type": "provider.free-limit",
                    "status": 429,
                    "message": "Free usage limit reached. Subscribe to OpenCode Go to keep going.",
                    "response": {
                        "body": "{\"type\":\"error\",\"error\":{\"type\":\"FreeUsageLimitError\"}}"
                    },
                    "retryAfterMs": 32400000
                }
            },
            {
                "id": "m4",
                "type": "idle",
                "outcome": "error",
                "error": {
                    "type": "provider.free-limit",
                    "status": 429,
                    "retryAfterMs": 32400000
                }
            },
            {
                "id": "m5",
                "type": "user",
                "time": {"created": 4000}
            },
            {
                "id": "m6",
                "type": "assistant",
                "time": {"created": 4500, "completed": 6000},
                "tokens": {"input": 800, "output": 15000, "reasoning": 5000, "cache": {"read": 250000, "write": 0}}
            }
        ]
    }"#;

    let export = parse_transcript_from_str(json_data).expect("Parsing valid synthetic transcript");
    let report = analyze_transcript(&export);

    assert_eq!(report.session_id.as_deref(), Some("ses_synth_001"));
    assert_eq!(report.title.as_deref(), Some("Synthetic Audit Test"));
    assert_eq!(report.model.as_deref(), Some("deepseek-v4.1-flash-free"));
    assert_eq!(report.total_messages, 6);
    assert_eq!(report.total_cycles, 2);

    // Cycle 1: msgs idx 0..1
    let c1 = &report.cycles[0];
    assert_eq!(c1.cycle_index, 1);
    assert_eq!(c1.start_msg_idx, 0);
    assert_eq!(c1.end_msg_idx, 1);
    assert_eq!(c1.total_messages, 2);
    assert_eq!(c1.assistant_messages, 2);
    assert_eq!(c1.input_tokens, 3500);
    assert_eq!(c1.output_tokens, 61000); // 35k + 26k
    assert_eq!(c1.reasoning_tokens, 22000); // 10k + 12k
    assert_eq!(c1.cache_read_tokens, 1100000);
    assert_eq!(c1.total_generated_tokens, 83000);
    assert_eq!(c1.total_billed_tokens, 64500);
    assert!(!c1.is_active);
    assert_eq!(c1.retry_after_ms, Some(32400000));

    // Cycle 2: msgs idx 4..5 (msg 2 and 3 were rate limit errors)
    let c2 = &report.cycles[1];
    assert_eq!(c2.cycle_index, 2);
    assert_eq!(c2.start_msg_idx, 4);
    assert_eq!(c2.end_msg_idx, 5);
    assert_eq!(c2.total_messages, 2);
    assert_eq!(c2.assistant_messages, 1);
    assert_eq!(c2.input_tokens, 800);
    assert_eq!(c2.output_tokens, 15000);
    assert_eq!(c2.reasoning_tokens, 5000);
    assert_eq!(c2.cache_read_tokens, 250000);
    assert!(c2.is_active);
    assert_eq!(c2.retry_after_ms, None);

    // Cutoff stats
    assert_eq!(report.average_cutoff_output, Some(61000));
    assert_eq!(report.min_cutoff_output, Some(61000));
    assert_eq!(report.max_cutoff_output, Some(61000));
    assert_eq!(report.active_cycle_remaining_tokens, Some(46000)); // 61000 - 15000
    assert_eq!(report.average_retry_after_ms, Some(32400000));
}

#[test]
fn test_real_transcript_forensic_accuracy() {
    let transcript_path = Path::new("overlays-r-els-a7-et-alias.json");
    if !transcript_path.exists() {
        eprintln!("Skipping real transcript test: file not found in current directory");
        return;
    }

    let export = parse_transcript_from_file(transcript_path)
        .expect("Valid real transcript overlays-r-els-a7-et-alias.json");
    let report = analyze_transcript(&export);

    assert_eq!(
        report.session_id.as_deref(),
        Some("ses_e2fdc9104ffez2EITe9H8lEZXI")
    );
    assert_eq!(report.total_messages, 663);
    assert_eq!(report.total_cycles, 4);

    // Proxy 1 (idx 0..155)
    let p1 = &report.cycles[0];
    assert_eq!(p1.cycle_index, 1);
    assert_eq!(p1.start_msg_idx, 0);
    assert_eq!(p1.end_msg_idx, 155);
    assert_eq!(p1.total_messages, 156);
    assert_eq!(p1.assistant_messages, 149);
    assert_eq!(p1.input_tokens, 150782);
    assert_eq!(p1.output_tokens, 61281);
    assert_eq!(p1.reasoning_tokens, 88889);
    assert_eq!(p1.cache_read_tokens, 27441024);
    assert_eq!(p1.total_generated_tokens, 150170);
    assert_eq!(p1.total_billed_tokens, 212063);
    assert!(!p1.is_active);
    assert_eq!(p1.retry_after_ms, Some(33076000));

    // Proxy 2 (idx 158..414)
    let p2 = &report.cycles[1];
    assert_eq!(p2.cycle_index, 2);
    assert_eq!(p2.start_msg_idx, 158);
    assert_eq!(p2.end_msg_idx, 414);
    assert_eq!(p2.total_messages, 257);
    assert_eq!(p2.assistant_messages, 247);
    assert_eq!(p2.input_tokens, 308658);
    assert_eq!(p2.output_tokens, 60189);
    assert_eq!(p2.reasoning_tokens, 54954);
    assert_eq!(p2.cache_read_tokens, 42562816);
    assert_eq!(p2.total_generated_tokens, 115143);
    assert_eq!(p2.total_billed_tokens, 368847);
    assert!(!p2.is_active);
    assert_eq!(p2.retry_after_ms, Some(32881000));

    // Proxy 3 (idx 417..613)
    let p3 = &report.cycles[2];
    assert_eq!(p3.cycle_index, 3);
    assert_eq!(p3.start_msg_idx, 417);
    assert_eq!(p3.end_msg_idx, 613);
    assert_eq!(p3.total_messages, 197);
    assert_eq!(p3.assistant_messages, 189);
    assert_eq!(p3.input_tokens, 352914);
    assert_eq!(p3.output_tokens, 65147);
    assert_eq!(p3.reasoning_tokens, 64939);
    assert_eq!(p3.cache_read_tokens, 36269952);
    assert_eq!(p3.total_generated_tokens, 130086);
    assert_eq!(p3.total_billed_tokens, 418061);
    assert!(!p3.is_active);
    assert_eq!(p3.retry_after_ms, Some(29108000));

    // Proxy 4 (idx 616..662)
    let p4 = &report.cycles[3];
    assert_eq!(p4.cycle_index, 4);
    assert_eq!(p4.start_msg_idx, 616);
    assert_eq!(p4.end_msg_idx, 662);
    assert_eq!(p4.total_messages, 47);
    assert_eq!(p4.assistant_messages, 41);
    assert_eq!(p4.input_tokens, 26783);
    assert_eq!(p4.output_tokens, 11284);
    assert_eq!(p4.reasoning_tokens, 12961);
    assert_eq!(p4.cache_read_tokens, 10310528);
    assert!(p4.is_active);
    assert_eq!(p4.retry_after_ms, None);

    // Forensic Averages
    assert_eq!(report.average_cutoff_output, Some(62205));
    assert_eq!(report.min_cutoff_output, Some(60189));
    assert_eq!(report.max_cutoff_output, Some(65147));
    assert_eq!(report.average_retry_after_ms, Some(31688333));
    assert_eq!(report.active_cycle_remaining_tokens, Some(50921)); // 62205 - 11284
}

#[test]
fn test_pool_size_estimation_rules() {
    // Zero tokens -> 1 min, 2 recommended
    let est0 = estimate_pool_size(0, None, None);
    assert_eq!(est0.min_proxies, 1);
    assert_eq!(est0.recommended_proxies, 2);
    assert_eq!(est0.quota_per_proxy, NOMINAL_OUTPUT_QUOTA_PER_IP);

    // Under quota (45k tokens)
    let est45k = estimate_pool_size(45_000, None, None);
    assert_eq!(est45k.min_proxies, 1);
    assert_eq!(est45k.recommended_proxies, 2);

    // Exact quota (60k tokens)
    let est60k = estimate_pool_size(60_000, None, None);
    assert_eq!(est60k.min_proxies, 1);
    assert_eq!(est60k.recommended_proxies, 2);

    // Just over quota (60,001 tokens) -> requires 2nd proxy
    let est60k1 = estimate_pool_size(60_001, None, None);
    assert_eq!(est60k1.min_proxies, 2);
    assert_eq!(est60k1.recommended_proxies, 3);

    // 150k tokens -> requires 3 proxies (60k + 60k + 30k)
    let est150k = estimate_pool_size(150_000, Some(450_000), None);
    assert_eq!(est150k.min_proxies, 3);
    assert_eq!(est150k.recommended_proxies, 4);
    assert_eq!(est150k.tokens_in, Some(450_000));

    // Custom quota per proxy
    let est_custom = estimate_pool_size(100_000, None, Some(30_000));
    assert_eq!(est_custom.quota_per_proxy, 30_000);
    assert_eq!(est_custom.min_proxies, 4); // ceil(100k / 30k) = 4
    assert_eq!(est_custom.recommended_proxies, 5);
}

#[test]
fn test_rate_limit_error_filtering() {
    // Status 429
    assert!(is_rate_limit_error(&SessionError {
        status: Some(429),
        ..Default::default()
    }));

    // Provider type free-limit
    assert!(is_rate_limit_error(&SessionError {
        r#type: Some("provider.free-limit".to_string()),
        ..Default::default()
    }));

    // FreeUsageLimitError in response body
    assert!(is_rate_limit_error(&SessionError {
        response: Some(SessionErrorResponse {
            body: Some("{\"error\":{\"type\":\"FreeUsageLimitError\"}}".to_string()),
        }),
        ..Default::default()
    }));

    // RateLimitError in response body
    assert!(is_rate_limit_error(&SessionError {
        response: Some(SessionErrorResponse {
            body: Some("{\"type\":\"RateLimitError\"}".to_string()),
        }),
        ..Default::default()
    }));

    // Non-rate-limit errors
    assert!(!is_rate_limit_error(&SessionError {
        status: Some(200),
        r#type: Some("provider.transport".to_string()),
        message: Some("Connection lost: ECONNRESET".to_string()),
        ..Default::default()
    }));

    assert!(!is_rate_limit_error(&SessionError {
        status: Some(500),
        message: Some("Internal Server Error".to_string()),
        ..Default::default()
    }));

    assert!(!is_rate_limit_error(&SessionError {
        status: Some(404),
        message: Some("Not Found".to_string()),
        ..Default::default()
    }));
}

#[test]
fn test_malformed_json_graceful_handling() {
    let broken_json = "{ invalid_json: true, ";
    let res = parse_transcript_from_str(broken_json);
    assert!(res.is_err());

    let empty_json = "{}";
    let res_empty = parse_transcript_from_str(empty_json);
    assert!(res_empty.is_ok());
    let report = analyze_transcript(&res_empty.unwrap());
    assert_eq!(report.total_messages, 0);
    assert_eq!(report.total_cycles, 0);
    assert_eq!(report.cycles.len(), 0);
}
