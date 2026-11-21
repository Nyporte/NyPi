//! Output truncation shared by tools (Pi's limits: 2000 lines or 50 KB, whichever comes first).

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Result of keeping the head or tail of a text.
#[derive(Debug, PartialEq)]
pub struct Truncation {
    pub text: String,
    pub truncated: bool,
    pub total_lines: usize,
    /// Lines kept, counted from the start (head) or end (tail).
    pub kept_lines: usize,
}

/// Keep whole lines from the start until either limit is reached.
pub fn truncate_head(text: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_lines = lines.len();
    let mut out = String::new();
    let mut kept = 0;
    for line in &lines {
        let extra = line.len() + usize::from(kept > 0);
        if kept >= max_lines || out.len() + extra > max_bytes {
            break;
        }
        if kept > 0 {
            out.push('\n');
        }
        out.push_str(line);
        kept += 1;
    }
    Truncation { truncated: kept < total_lines, text: out, total_lines, kept_lines: kept }
}

/// Keep whole lines from the end until either limit is reached (used for command output,
/// where the end usually holds the error).
pub fn truncate_tail(text: &str, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_lines = lines.len();
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for line in lines.iter().rev() {
        let extra = line.len() + usize::from(!kept.is_empty());
        if kept.len() >= max_lines || bytes + extra > max_bytes {
            break;
        }
        bytes += extra;
        kept.push(line);
    }
    // A single final line longer than the byte limit still shows its end.
    if kept.is_empty()
        && let Some(last) = lines.last()
    {
        let mut start = last.len().saturating_sub(max_bytes);
        while !last.is_char_boundary(start) {
            start += 1;
        }
        kept.push(&last[start..]);
    }
    kept.reverse();
    let kept_lines = kept.len();
    Truncation { text: kept.join("\n"), truncated: kept_lines < total_lines, total_lines, kept_lines }
}

/// Shorten a single line to `max` characters.
pub fn truncate_line(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let cut: String = line.chars().take(max).collect();
    format!("{cut}... [truncated]")
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_and_tail_respect_line_and_byte_limits() {
        let text = "a\nb\nc\nd";
        assert_eq!(truncate_head(text, 2, 100).text, "a\nb");
        assert_eq!(truncate_tail(text, 2, 100).text, "c\nd");
        assert_eq!(truncate_head(text, 10, 3).text, "a\nb");
        let full = truncate_head(text, 10, 100);
        assert!(!full.truncated);
        assert_eq!(full.total_lines, 4);
        assert_eq!(truncate_tail("xxxxxxxx", 10, 3).text, "xxx");
    }
}
