pub mod checks;
pub mod evaluator;
pub mod parser;

/// A single acceptance criterion parsed from the task body.
#[derive(Debug, Clone)]
pub struct Criterion {
    /// Shell command to execute.
    pub command: String,
    /// What to assert on the result.
    pub check: CheckType,
}

/// The type of assertion to run against a command's output.
#[derive(Debug, Clone)]
pub enum CheckType {
    ExitCode(i32),
    OutputContains(String),
    OutputMatches(String), // regex
    FileExists(String),
    FileContains { path: String, expected: String },
    FileSizeRange { path: String, min: u64, max: u64 },
    HttpStatus { url: String, expected: u16 },
    JsonPath { path: String, expected: String },
    NotContains(String),
}

/// Result of running a single criterion check.
#[derive(Debug, Clone)]
pub struct CriterionResult {
    /// Human-readable description of what was checked.
    pub criterion_display: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Actual value observed.
    pub actual: String,
}

/// Aggregate result of running all criteria for a task.
#[derive(Debug, Clone)]
pub struct EvalResult {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub results: Vec<CriterionResult>,
}

impl EvalResult {
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }

    /// Full markdown summary with checkmarks for all criteria.
    pub fn full_summary(&self) -> String {
        let mut out = format!(
            "**{}/{} checks passed**\n\n",
            self.passed, self.total
        );
        for r in &self.results {
            let mark = if r.passed { "pass" } else { "FAIL" };
            out.push_str(&format!("- [{}] {}\n", mark, r.criterion_display));
            if !r.passed {
                out.push_str(&format!("  actual: {}\n", r.actual));
            }
        }
        out
    }

    /// Markdown listing only failed criteria (for retry context).
    pub fn failure_summary(&self) -> String {
        let mut out = format!(
            "**{}/{} checks failed**\n\n",
            self.failed, self.total
        );
        for r in &self.results {
            if !r.passed {
                out.push_str(&format!("- [FAIL] {}\n", r.criterion_display));
                out.push_str(&format!("  actual: {}\n", r.actual));
            }
        }
        out
    }
}

pub use evaluator::Evaluator;
