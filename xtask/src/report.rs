use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Passed,
    Failed,
    Timeout,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepResult {
    pub name: String,
    pub status: Status,
    pub code: Option<i32>,
    pub duration_ms: u128,
    pub log: Option<String>,
    pub note: Option<String>,
    /// Collection / shutdown steps: their failure is noted but does not change the verdict.
    #[serde(skip)]
    pub always: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    pub failed_checks: Vec<String>,
    pub reason: Option<String>,
}

/// `passed` only if no normal step fails AND `client.json` is valid
/// with a `checks` array without `passed: false`.
pub fn verdict(steps: &[StepResult], client_json: Option<&str>) -> Verdict {
    let mut v = Verdict {
        passed: false,
        failed_checks: vec![],
        reason: None,
    };
    if let Some(s) = steps
        .iter()
        .find(|s| !s.always && s.status != Status::Passed && s.status != Status::Skipped)
    {
        v.reason = Some(format!("step {} : {:?}", s.name, s.status));
        return v;
    }
    let Some(text) = client_json else {
        v.reason = Some("client.json missing".into());
        return v;
    };
    let Ok(json) = serde_json::from_str::<Value>(text) else {
        v.reason = Some("client.json invalid".into());
        return v;
    };
    let Some(checks) = json.get("checks").and_then(Value::as_array) else {
        v.reason = Some("client.json without checks array".into());
        return v;
    };
    for c in checks {
        match c.get("passed").and_then(Value::as_bool) {
            Some(true) => {}
            _ => v.failed_checks.push(
                c.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
            ),
        }
    }
    if checks.is_empty() {
        v.reason = Some("checks empty".into());
        return v;
    }
    v.passed = v.failed_checks.is_empty();
    if !v.passed {
        v.reason = Some("checks failed".into());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, status: Status, always: bool) -> StepResult {
        StepResult {
            name: name.into(),
            status,
            code: None,
            duration_ms: 0,
            log: None,
            note: None,
            always,
        }
    }
    const OK: &str = r#"{"checks":[{"name":"a","passed":true,"detail":""}],"metrics":{}}"#;

    #[test]
    fn passes_when_all_good() {
        assert!(verdict(&[step("build", Status::Passed, false)], Some(OK)).passed);
    }

    #[test]
    fn fails_on_step_failure_or_timeout() {
        assert!(!verdict(&[step("deploy", Status::Failed, false)], Some(OK)).passed);
        assert!(!verdict(&[step("client", Status::Timeout, false)], Some(OK)).passed);
    }

    #[test]
    fn collection_failure_does_not_change_verdict() {
        assert!(verdict(&[step("console", Status::Failed, true)], Some(OK)).passed);
    }

    #[test]
    fn rejects_missing_invalid_or_failing_client_json() {
        assert!(!verdict(&[], None).passed);
        assert!(!verdict(&[], Some("{")).passed);
        assert!(!verdict(&[], Some(r#"{"metrics":{}}"#)).passed);
        assert!(!verdict(&[], Some(r#"{"checks":[]}"#)).passed);
        let bad = r#"{"checks":[{"name":"x","passed":false,"detail":""}]}"#;
        let v = verdict(&[], Some(bad));
        assert!(!v.passed);
        assert_eq!(v.failed_checks, vec!["x"]);
    }
}
