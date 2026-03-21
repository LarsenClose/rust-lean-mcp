pub mod batch;
pub mod batch_goals;
pub mod build;
pub mod code_actions;
pub mod completions;
pub mod declarations;
pub mod diagnostics;
pub mod goal;
pub mod hover;
pub mod multi_attempt;
pub mod outline;
pub mod profile;
pub mod project_health;
pub mod proof_diff;
pub mod references;
pub mod run_code;
pub mod search;
pub mod symbol_resolve;
pub mod verify;
pub mod widgets;

/// Prepend `set_option maxHeartbeats N` to code for elaboration timeout control.
///
/// Controlled by the `LEAN_MCP_MAX_HEARTBEATS` environment variable:
/// - Unset or non-zero value: prepends `set_option maxHeartbeats <value>` (default 400000)
/// - `"0"`: disables injection entirely (no limit)
///
/// If the code already contains `maxHeartbeats`, no injection is performed
/// to avoid overriding the user's explicit setting.
pub fn prepend_max_heartbeats(code: &str) -> String {
    let max_hb = std::env::var("LEAN_MCP_MAX_HEARTBEATS").unwrap_or_else(|_| "400000".to_string());

    if max_hb == "0" || code.contains("maxHeartbeats") {
        return code.to_string();
    }

    if code.is_empty() {
        format!("set_option maxHeartbeats {max_hb}")
    } else {
        format!("set_option maxHeartbeats {max_hb}\n{code}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepend_max_heartbeats_adds_default() {
        // Clear any env override for a clean test
        std::env::remove_var("LEAN_MCP_MAX_HEARTBEATS");

        let result = prepend_max_heartbeats("def foo := 42");
        assert!(result.starts_with("set_option maxHeartbeats 400000\n"));
        assert!(result.ends_with("def foo := 42"));
    }

    #[test]
    fn prepend_max_heartbeats_skips_when_already_present() {
        std::env::remove_var("LEAN_MCP_MAX_HEARTBEATS");

        let code = "set_option maxHeartbeats 200000\ndef foo := 42";
        let result = prepend_max_heartbeats(code);
        assert_eq!(result, code);
    }

    #[test]
    fn prepend_max_heartbeats_disabled_with_zero() {
        std::env::set_var("LEAN_MCP_MAX_HEARTBEATS", "0");

        let code = "def foo := 42";
        let result = prepend_max_heartbeats(code);
        assert_eq!(result, code);

        // Clean up
        std::env::remove_var("LEAN_MCP_MAX_HEARTBEATS");
    }

    #[test]
    fn prepend_max_heartbeats_uses_custom_env_value() {
        std::env::set_var("LEAN_MCP_MAX_HEARTBEATS", "800000");

        let result = prepend_max_heartbeats("def foo := 42");
        assert!(result.starts_with("set_option maxHeartbeats 800000\n"));

        // Clean up
        std::env::remove_var("LEAN_MCP_MAX_HEARTBEATS");
    }
}
