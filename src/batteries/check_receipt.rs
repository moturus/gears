//! A check's evidence is only the receipt its tool actually returned.

use crate::provider::ToolOutcome;

use crate::batteries::{Event, check::Receipt};

pub fn completed(event: &Event) -> Result<Option<Receipt>, String> {
    if event.payload["outcome"] != serde_json::to_value(ToolOutcome::Completed).unwrap()
        || event.payload["tool_result"]["is_error"] != false
        || event.payload["tool_result_truncated"] == true
    {
        return Ok(None);
    }
    let Some(content) = event.payload["tool_result"]["content"].as_str() else {
        return Ok(None);
    };
    let Ok(receipt) = serde_json::from_str::<Receipt>(content) else {
        return Ok(None);
    };
    if receipt.version != 1
        || receipt.kind != "check"
        || receipt.passed != (receipt.exit_status == Some(0))
    {
        return Err("invalid check receipt".into());
    }
    Ok(Some(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(passed: bool, exit_status: Option<i32>) -> Receipt {
        Receipt {
            version: 1,
            kind: "check".into(),
            argv: vec!["program".into()],
            system: crate::batteries::check_config::system().into(),
            exit_status,
            duration_ms: 1,
            passed,
            stdout: String::new(),
            stderr: String::new(),
            output_truncated: false,
            output_file: None,
        }
    }

    fn event(payload: serde_json::Value) -> Event {
        Event {
            protocol_version: 1,
            event: "entry".into(),
            workspace: "/none/workspace".into(),
            session: crate::batteries::Session {
                id: "s1".into(),
                path: None,
                state: None,
            },
            payload,
        }
    }

    #[test]
    fn only_a_completed_untruncated_receipt_counts_as_evidence() {
        let denied = event(serde_json::json!({"outcome": "not_started",
            "tool_result": {"content": "denied", "is_error": true}}));
        assert!(completed(&denied).unwrap().is_none());
        let truncated = event(serde_json::json!({"outcome": "completed",
            "tool_result": {"content": "{}", "is_error": false}, "tool_result_truncated": true}));
        assert!(completed(&truncated).unwrap().is_none());
        let text = event(serde_json::json!({"outcome": "completed",
            "tool_result": {"content": "not a receipt", "is_error": false}}));
        assert!(completed(&text).unwrap().is_none());
        let good = event(serde_json::json!({"outcome": "completed",
            "tool_result": {"content": serde_json::to_string(&receipt(true, Some(0))).unwrap(),
                "is_error": false}}));
        assert!(completed(&good).unwrap().unwrap().passed);
        let failed = event(serde_json::json!({"outcome": "completed",
            "tool_result": {"content": serde_json::to_string(&receipt(false, Some(1))).unwrap(),
                "is_error": false}}));
        assert!(!completed(&failed).unwrap().unwrap().passed);
        let inconsistent = event(serde_json::json!({"outcome": "completed",
            "tool_result": {"content": serde_json::to_string(&receipt(true, Some(1))).unwrap(),
                "is_error": false}}));
        assert!(completed(&inconsistent).is_err());
    }
}
