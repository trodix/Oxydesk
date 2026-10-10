use std::collections::BTreeMap;

use serde::Serialize;

use crate::stats::round1;

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub metrics: BTreeMap<String, f64>,
}

impl Report {
    pub fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
    }

    pub fn metric(&mut self, name: &str, value: f64) {
        self.metrics.insert(name.into(), round1(value));
    }

    pub fn passed(&self, name: &str) -> bool {
        self.checks.iter().any(|c| c.name == name && c.passed)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serializable")
    }
}
