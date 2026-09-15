//! Five-hour usage-quota probing shared by harness implementations and the
//! auto-resume watcher.
//!
//! Only the rolling ~5h window matters here. Weekly / secondary windows are
//! parsed when present so callers can ignore them, but never drive auto-resume.

use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Outcome of probing a harness's five-hour usage window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaProbeResult {
    /// Window has remaining capacity (used < 100%).
    Available {
        used_percent: u8,
        reset_at_ms: Option<i64>,
    },
    /// Window is exhausted; wait until `reset_at_ms` (unix ms) before retrying.
    Exhausted { reset_at_ms: Option<i64> },
    /// Probe ran but could not interpret the response.
    Unknown { detail: String },
    /// This harness / installation cannot probe five-hour quota.
    Unsupported,
}

impl QuotaProbeResult {
    pub fn is_exhausted(&self) -> bool {
        matches!(self, Self::Exhausted { .. })
    }

    pub fn reset_at_ms(&self) -> Option<i64> {
        match self {
            Self::Available { reset_at_ms, .. } | Self::Exhausted { reset_at_ms } => *reset_at_ms,
            _ => None,
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse Claude Code `claude -p /usage --output-format json` (or raw `/usage` text).
///
/// Claude's "Current session" line is the ~5h window; "Current week" is ignored.
pub fn parse_claude_usage_text(text: &str) -> QuotaProbeResult {
    let owned = extract_claude_result_text(text);
    let body = owned.as_deref().unwrap_or(text);

    let session = match find_percent_after_label(body, "Current session") {
        Some(v) => v,
        None => {
            return QuotaProbeResult::Unknown {
                detail: "no Current session line in claude /usage".into(),
            };
        }
    };
    let reset_at_ms = parse_claude_reset_on_line(body, "Current session");

    if session >= 100 {
        QuotaProbeResult::Exhausted { reset_at_ms }
    } else {
        QuotaProbeResult::Available {
            used_percent: session.min(100) as u8,
            reset_at_ms,
        }
    }
}

fn extract_claude_result_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('{') || !trimmed.contains("result") {
        return None;
    }
    let value: Value = serde_json::from_str(trimmed).ok()?;
    value
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn find_percent_after_label(text: &str, label: &str) -> Option<u32> {
    for line in text.lines() {
        let line = line.trim();
        if !line.contains(label) {
            continue;
        }
        let rest = line.split(label).nth(1)?;
        let rest = rest.trim().trim_start_matches(':').trim();
        let pct = rest.split('%').next()?.trim();
        if let Ok(n) = pct.parse::<u32>() {
            return Some(n);
        }
    }
    None
}

fn parse_claude_reset_on_line(text: &str, label: &str) -> Option<i64> {
    for line in text.lines() {
        if !line.contains(label) {
            continue;
        }
        if let Some(idx) = line.find("resets ") {
            let phrase = line[idx + "resets ".len()..].trim();
            // Numeric unix seconds / ms only; human dates left as None.
            let token = phrase.split_whitespace().next()?;
            if let Ok(secs) = token.parse::<i64>() {
                return Some(if secs > 10_000_000_000 {
                    secs
                } else {
                    secs * 1000
                });
            }
        }
    }
    None
}

/// Parse `agy -p /usage` tabular text. Uses a "Five Hour Limit" row.
/// Prefer a Claude/GPT row when several exist.
pub fn parse_agy_usage_text(text: &str) -> QuotaProbeResult {
    let mut chosen: Option<(u32, Option<i64>, bool)> = None;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("five hour") {
            continue;
        }
        let cols: Vec<&str> = if line.contains('\t') {
            line.split('\t').map(str::trim).collect()
        } else {
            line.split_whitespace().collect()
        };
        let mut remaining: Option<u32> = None;
        let mut reset_at_ms: Option<i64> = None;
        for col in &cols {
            if let Some(pct) = col.strip_suffix('%') {
                if let Ok(n) = pct.parse::<u32>() {
                    remaining = Some(n);
                }
            }
            if let Some(ms) = parse_iso8601_to_ms(col) {
                reset_at_ms = Some(ms);
            }
        }
        let Some(rem) = remaining else { continue };
        let prefer = lower.contains("claude") || lower.contains("gpt");
        match chosen {
            None => chosen = Some((rem, reset_at_ms, prefer)),
            Some((_, _, was_pref)) if prefer && !was_pref => {
                chosen = Some((rem, reset_at_ms, prefer));
            }
            _ => {}
        }
        if prefer {
            break;
        }
    }
    match chosen {
        None => QuotaProbeResult::Unknown {
            detail: "no Five Hour Limit row in agy /usage".into(),
        },
        Some((remaining, reset_at_ms, _)) => {
            let used = 100u32.saturating_sub(remaining);
            if remaining == 0 || used >= 100 {
                QuotaProbeResult::Exhausted { reset_at_ms }
            } else {
                QuotaProbeResult::Available {
                    used_percent: used.min(100) as u8,
                    reset_at_ms,
                }
            }
        }
    }
}

fn parse_iso8601_to_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 20 || !s.contains('T') {
        return None;
    }
    let (date, time_z) = s.split_once('T')?;
    let mut d = date.split('-');
    let y: i32 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let time = time_z.trim_end_matches('Z');
    let time = time.split('.').next().unwrap_or(time);
    let mut t = time.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let mi: u32 = t.next()?.parse().ok()?;
    let sec: u32 = t.next()?.parse().ok()?;
    days_from_civil(y, mo, day).map(|days| {
        let secs = days * 86400 + (h as i64) * 3600 + (mi as i64) * 60 + sec as i64;
        secs * 1000
    })
}

fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Parse Codex `account/rateLimits/read` JSON (same shape as `codex-limits --json`).
/// Uses `primary` (typically 300 minutes) only; ignores `secondary` weekly.
pub fn parse_codex_rate_limits_json(text: &str) -> QuotaProbeResult {
    let value: Value = match serde_json::from_str(text.trim()) {
        Ok(v) => v,
        Err(err) => {
            return QuotaProbeResult::Unknown {
                detail: format!("codex rate-limits JSON: {err}"),
            };
        }
    };
    let Some(primary) = extract_codex_primary(&value) else {
        return QuotaProbeResult::Unknown {
            detail: "no primary five-hour window in codex rate limits".into(),
        };
    };
    let used = primary
        .get("usedPercent")
        .and_then(Value::as_f64)
        .map(|p| p.round().clamp(0.0, 100.0) as u32)
        .unwrap_or(0);
    let reset_at_ms = primary.get("resetsAt").and_then(Value::as_i64).map(|secs| {
        if secs > 10_000_000_000 {
            secs
        } else {
            secs * 1000
        }
    });
    // Only the primary (~5h) window drives auto-resume; ignore weekly/secondary
    // and any aggregate rateLimitReachedType that may refer to them.
    if used >= 100 {
        QuotaProbeResult::Exhausted { reset_at_ms }
    } else {
        QuotaProbeResult::Available {
            used_percent: used.min(100) as u8,
            reset_at_ms,
        }
    }
}

fn extract_codex_primary(value: &Value) -> Option<&Value> {
    if let Some(p) = value.pointer("/rateLimits/primary") {
        return Some(p);
    }
    if let Some(p) = value.get("primary") {
        return Some(p);
    }
    if let Some(map) = value.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for row in map.values() {
            if let Some(p) = row.get("primary") {
                let mins = p
                    .get("windowDurationMins")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if (200..=400).contains(&mins) || mins == 0 {
                    return Some(p);
                }
            }
        }
        for row in map.values() {
            if let Some(p) = row.get("primary") {
                return Some(p);
            }
        }
    }
    None
}

/// Hint whether a turn failure looks like five-hour quota exhaustion (probe still confirms).
pub fn looks_like_five_hour_quota_failure(error_kind: &str, error_message: &str) -> bool {
    let kind = error_kind.to_ascii_lowercase();
    let msg = error_message.to_ascii_lowercase();
    kind.contains("usage_limit")
        || kind.contains("rate_limit")
        || msg.contains("usage limit")
        || msg.contains("rate limit")
        || msg.contains("out of codex")
        || msg.contains("you've hit your limit")
        || msg.contains("limit reached")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_available_from_json_envelope() {
        let raw = r#"{"type":"result","local_command":"usage","result":"Current session: 17% used\nCurrent week (all models): 40% used · resets Sep 21, 9:59pm (UTC)","usage":{"input_tokens":0}}"#;
        match parse_claude_usage_text(raw) {
            QuotaProbeResult::Available {
                used_percent: 17, ..
            } => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn claude_exhausted_from_plain_text() {
        let raw = "Current session: 100% used · resets Sep 16, 3:00am (UTC)\nCurrent week (all models): 50% used";
        assert!(matches!(
            parse_claude_usage_text(raw),
            QuotaProbeResult::Exhausted { .. }
        ));
    }

    #[test]
    fn agy_five_hour_remaining() {
        let raw = "Gemini Models\tWeekly Limit Remaining\t94%\t2026-09-18T10:08:41Z\n\
Gemini Models\tFive Hour Limit Remaining\t100%\t2026-09-16T02:47:26Z\n\
Claude and GPT models\tWeekly Limit Remaining\t3%\t2026-09-20T21:00:14Z\n\
Claude and GPT models\tFive Hour Limit Remaining\t0%\t2026-09-16T02:47:26Z\n";
        match parse_agy_usage_text(raw) {
            QuotaProbeResult::Exhausted {
                reset_at_ms: Some(ms),
            } => assert!(ms > 0),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn agy_available() {
        let raw = "Claude and GPT models\tFive Hour Limit Remaining\t55%\t2026-09-16T02:47:26Z\n";
        match parse_agy_usage_text(raw) {
            QuotaProbeResult::Available {
                used_percent: 45, ..
            } => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn codex_primary_exhausted_ignores_weekly() {
        let raw = r#"{
          "rateLimits": {
            "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1789524596},
            "secondary": {"usedPercent": 79, "windowDurationMins": 10080, "resetsAt": 1790008653},
            "rateLimitReachedType": "rate_limit_reached"
          }
        }"#;
        match parse_codex_rate_limits_json(raw) {
            QuotaProbeResult::Exhausted {
                reset_at_ms: Some(ms),
            } => {
                assert_eq!(ms, 1789524596 * 1000);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn codex_available() {
        let raw = r#"{"rateLimits":{"primary":{"usedPercent":12.4,"windowDurationMins":300,"resetsAt":1789524596}}}"#;
        match parse_codex_rate_limits_json(raw) {
            QuotaProbeResult::Available {
                used_percent: 12, ..
            } => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn quota_failure_hint() {
        assert!(looks_like_five_hour_quota_failure(
            "claude_usage_limit",
            "Claude Code usage limit reached"
        ));
        assert!(!looks_like_five_hour_quota_failure(
            "tool_error",
            "command failed"
        ));
    }
}
