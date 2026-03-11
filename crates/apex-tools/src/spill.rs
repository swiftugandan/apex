use std::io;
use std::path::PathBuf;

use apex_core::domain::{OutputStats, SpillStrategy};

/// Default spill configuration used by shell_exec and custom_tools.
pub const DEFAULT_SPILL_STRATEGY: SpillStrategy = SpillStrategy::HeadTail;
pub const DEFAULT_SPILL_HEAD_LINES: usize = 20;
pub const DEFAULT_SPILL_TAIL_LINES: usize = 20;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 16384;

/// Result of spilling output to disk.
pub struct SpillResult {
    pub path: String,
    pub envelope: String,
    pub stats: OutputStats,
}

/// Entry in the scratch directory listing.
pub struct SpillEntry {
    pub path: String,
    pub size: u64,
}

/// Manages spilling large tool output to disk with summary envelopes.
pub struct SpillManager {
    scratch_dir: PathBuf,
}

impl SpillManager {
    pub fn new(scratch_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&scratch_dir);
        Self { scratch_dir }
    }

    /// Spill output to disk if it exceeds max_bytes.
    /// Returns None if output fits within the limit.
    pub fn spill_if_needed(
        &self,
        output: &str,
        max_bytes: usize,
        strategy: SpillStrategy,
        head_lines: usize,
        tail_lines: usize,
    ) -> Option<SpillResult> {
        if output.len() <= max_bytes {
            return None;
        }

        // Generate a short hex filename
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let short_hex = format!("{:012x}", now & 0xFFFF_FFFF_FFFF);
        let filename = format!("result-{short_hex}.txt");
        let path = self.scratch_dir.join(&filename);

        // Write full output to file
        if let Err(e) = std::fs::write(&path, output) {
            eprintln!("spill: failed to write {}: {e}", path.display());
            return None;
        }

        // Compute stats
        let stats = compute_stats(output);

        // Format summary envelope
        let lines: Vec<&str> = output.lines().collect();
        let envelope = match strategy {
            SpillStrategy::HeadTail => {
                let head: Vec<&str> = lines.iter().take(head_lines).copied().collect();
                let tail: Vec<&str> = if lines.len() > head_lines + tail_lines {
                    lines[lines.len() - tail_lines..].to_vec()
                } else if lines.len() > head_lines {
                    lines[head_lines..].to_vec()
                } else {
                    vec![]
                };

                let mut env = format!(
                    "[output spilled to {}]\n[{} lines, {} bytes]\n",
                    filename, stats.total_lines, stats.total_bytes
                );
                if !stats.patterns.is_empty() {
                    let pattern_summary: Vec<String> = stats
                        .patterns
                        .iter()
                        .map(|(name, count)| format!("{name}: {count}"))
                        .collect();
                    env.push_str(&format!("[patterns: {}]\n", pattern_summary.join(", ")));
                }
                env.push_str("\n--- HEAD ---\n");
                for line in &head {
                    env.push_str(line);
                    env.push('\n');
                }
                if !tail.is_empty() {
                    env.push_str(&format!(
                        "\n... ({} lines omitted) ...\n\n--- TAIL ---\n",
                        lines.len().saturating_sub(head_lines + tail_lines)
                    ));
                    for line in &tail {
                        env.push_str(line);
                        env.push('\n');
                    }
                }
                env
            }
            SpillStrategy::TailOnly => {
                let tail: Vec<&str> = if lines.len() > tail_lines {
                    lines[lines.len() - tail_lines..].to_vec()
                } else {
                    lines.clone()
                };

                let mut env = format!(
                    "[output spilled to {}]\n[{} lines, {} bytes]\n",
                    filename, stats.total_lines, stats.total_bytes
                );
                if !stats.patterns.is_empty() {
                    let pattern_summary: Vec<String> = stats
                        .patterns
                        .iter()
                        .map(|(name, count)| format!("{name}: {count}"))
                        .collect();
                    env.push_str(&format!("[patterns: {}]\n", pattern_summary.join(", ")));
                }
                env.push_str(&format!(
                    "\n... ({} lines omitted) ...\n\n--- TAIL ---\n",
                    lines.len().saturating_sub(tail_lines)
                ));
                for line in &tail {
                    env.push_str(line);
                    env.push('\n');
                }
                env
            }
        };

        let path_str = path.to_string_lossy().to_string();
        Some(SpillResult {
            path: path_str,
            envelope,
            stats,
        })
    }

    /// Delete a specific spill file.
    #[allow(dead_code)] // Public API; used in tests and for future single-file cleanup
    pub fn delete(&self, spill_path: &str) -> io::Result<()> {
        std::fs::remove_file(spill_path)
    }

    /// Remove all files in the scratch directory. Returns number of files removed.
    pub fn clean_all(&self) -> io::Result<u32> {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&self.scratch_dir) {
            for entry in entries.flatten() {
                if entry.path().is_file() {
                    std::fs::remove_file(entry.path())?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// List all spill files in the scratch directory.
    pub fn list(&self) -> io::Result<Vec<SpillEntry>> {
        let mut entries = Vec::new();
        if let Ok(dir) = std::fs::read_dir(&self.scratch_dir) {
            for entry in dir.flatten() {
                if entry.path().is_file() {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    entries.push(SpillEntry {
                        path: entry.path().to_string_lossy().to_string(),
                        size,
                    });
                }
            }
        }
        Ok(entries)
    }
}

/// Scan output for common error/warning patterns and compute stats.
fn compute_stats(output: &str) -> OutputStats {
    let total_lines = output.lines().count() as u64;
    let total_bytes = output.len() as u64;

    let patterns_to_check = [("ERROR", "error"), ("WARNING", "warning"), ("FAIL", "fail")];

    let mut patterns = Vec::new();
    for (label, needle) in &patterns_to_check {
        let count = output
            .lines()
            .filter(|l| {
                let lower = l.to_lowercase();
                lower.contains(needle)
            })
            .count() as u32;
        if count > 0 {
            patterns.push((label.to_string(), count));
        }
    }

    OutputStats {
        total_lines,
        total_bytes,
        patterns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> (tempfile::TempDir, SpillManager) {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        let mgr = SpillManager::new(scratch);
        (dir, mgr)
    }

    #[test]
    fn no_spill_when_under_limit() {
        let (_dir, mgr) = temp_dir();
        let result = mgr.spill_if_needed("small output", 1000, SpillStrategy::HeadTail, 5, 5);
        assert!(result.is_none());
    }

    #[test]
    fn spills_when_over_limit() {
        let (_dir, mgr) = temp_dir();
        let big = "line\n".repeat(1000);
        let result = mgr
            .spill_if_needed(&big, 100, SpillStrategy::HeadTail, 3, 3)
            .unwrap();
        assert!(result.path.contains("result-"));
        assert!(result.envelope.contains("[output spilled to"));
        assert!(result.envelope.contains("--- HEAD ---"));
        assert!(result.envelope.contains("--- TAIL ---"));
        assert_eq!(result.stats.total_lines, 1000);
        // Verify file exists
        assert!(std::path::Path::new(&result.path).exists());
    }

    #[test]
    fn tail_only_strategy() {
        let (_dir, mgr) = temp_dir();
        let big = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = mgr
            .spill_if_needed(&big, 100, SpillStrategy::TailOnly, 5, 5)
            .unwrap();
        assert!(result.envelope.contains("--- TAIL ---"));
        assert!(!result.envelope.contains("--- HEAD ---"));
        assert!(result.envelope.contains("line 100"));
    }

    #[test]
    fn clean_all_removes_files() {
        let (_dir, mgr) = temp_dir();
        let big = "x".repeat(200);
        mgr.spill_if_needed(&big, 100, SpillStrategy::HeadTail, 1, 1);
        mgr.spill_if_needed(&big, 100, SpillStrategy::HeadTail, 1, 1);
        let count = mgr.clean_all().unwrap();
        assert!(count >= 1);
        let entries = mgr.list().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn list_returns_entries() {
        let (_dir, mgr) = temp_dir();
        let big = "x".repeat(200);
        mgr.spill_if_needed(&big, 100, SpillStrategy::HeadTail, 1, 1);
        let entries = mgr.list().unwrap();
        assert!(!entries.is_empty());
        assert!(entries[0].size > 0);
    }

    #[test]
    fn stats_detect_patterns() {
        let output = "OK\nERROR: something\nWARNING: watch out\nOK\nFAILED test\n";
        let stats = compute_stats(output);
        assert_eq!(stats.total_lines, 5);
        assert!(!stats.patterns.is_empty());
        // Should detect ERROR, WARNING, and FAIL
        assert!(stats.patterns.iter().any(|(name, _)| name == "ERROR"));
        assert!(stats.patterns.iter().any(|(name, _)| name == "WARNING"));
        assert!(stats.patterns.iter().any(|(name, _)| name == "FAIL"));
    }

    #[test]
    fn delete_removes_file() {
        let (_dir, mgr) = temp_dir();
        let big = "x".repeat(200);
        let result = mgr
            .spill_if_needed(&big, 100, SpillStrategy::HeadTail, 1, 1)
            .unwrap();
        assert!(std::path::Path::new(&result.path).exists());
        mgr.delete(&result.path).unwrap();
        assert!(!std::path::Path::new(&result.path).exists());
    }
}
