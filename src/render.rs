//! Pure rendering helpers: the status tree, PR titles, and the shared stack
//! navigation block injected into every PR body. Kept side-effect-free so they
//! can be unit-tested without git or the network.

pub const BEGIN: &str = "<!-- git-stack:begin -->";
pub const END: &str = "<!-- git-stack:end -->";

/// One entry in a stack line for rendering purposes.
pub struct Entry {
    pub branch: String,
    pub pr: Option<PrRef>,
    /// The branch currently holds persisted conflict markers.
    pub conflicted: bool,
}

#[derive(Clone)]
pub struct PrRef {
    pub number: u64,
    pub url: String,
    pub state: String,
}

/// Numbered title: `[k/n] <subject>`, stripping any prior `[i/j] ` prefix so
/// re-submitting doesn't stack prefixes.
pub fn numbered_title(subject: &str, index: usize, total: usize) -> String {
    format!("[{}/{}] {}", index + 1, total, strip_prefix(subject))
}

fn strip_prefix(subject: &str) -> &str {
    let s = subject.trim_start();
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(close) = rest.find(']') {
            let inside = &rest[..close];
            if inside.contains('/') && inside.chars().all(|c| c.is_ascii_digit() || c == '/') {
                return rest[close + 1..].trim_start();
            }
        }
    }
    s
}

/// Build the shared navigation block listing every PR in the line, marking the
/// one for `current`. Top of stack listed first (matches how reviewers read).
pub fn nav_block(line: &[Entry], current: &str, trunk: &str) -> String {
    let mut lines = vec!["### 📚 Stack".to_string(), String::new()];
    let total = line.len();
    for (i, e) in line.iter().enumerate().rev() {
        let number = total - i; // 1-based from the bottom
        let marker = if e.branch == current {
            " 👈 **this PR**"
        } else {
            ""
        };
        let target = if i == 0 {
            trunk.to_string()
        } else {
            line[i - 1].branch.clone()
        };
        let pr_txt = match &e.pr {
            Some(p) => format!("#{}", p.number),
            None => "(not submitted)".to_string(),
        };
        lines.push(format!(
            "{}. {} `{}` → `{}`{}",
            number, pr_txt, e.branch, target, marker
        ));
    }
    lines.push(String::new());
    lines.push("<sub>Managed by git-stack.</sub>".to_string());
    lines.join("\n")
}

/// Combine a user-authored body with the nav block, replacing any previous
/// block bounded by the BEGIN/END markers.
pub fn compose_body(user_body: &str, nav: &str) -> String {
    let cleaned = strip_block(user_body);
    let cleaned = cleaned.trim_end();
    if cleaned.is_empty() {
        format!("{BEGIN}\n{nav}\n{END}")
    } else {
        format!("{cleaned}\n\n{BEGIN}\n{nav}\n{END}")
    }
}

/// Remove a previously injected BEGIN..END block (inclusive) from `body`.
pub fn strip_block(body: &str) -> String {
    match (body.find(BEGIN), body.find(END)) {
        (Some(start), Some(end)) if end >= start => {
            let after = end + END.len();
            let mut result = String::new();
            result.push_str(&body[..start]);
            result.push_str(&body[after..]);
            result
        }
        _ => body.to_string(),
    }
}

/// Render the status tree, top of stack first, marking `current`.
/// `entries` is bottom-first; `trunk` is shown as the base.
pub fn status_tree(
    entries: &[Entry],
    current: &str,
    trunk: &str,
    fork_note: Option<&str>,
) -> String {
    let mut out = String::new();
    for e in entries.iter().rev() {
        let node = if e.branch == current { "◉" } else { "◯" };
        let pr = match &e.pr {
            Some(p) => format!("  #{} [{}]", p.number, p.state),
            None => String::new(),
        };
        let here = if e.branch == current {
            "  ← current"
        } else {
            ""
        };
        let warn = if e.conflicted {
            "  ⚠ conflict markers"
        } else {
            ""
        };
        out.push_str(&format!("{node} {}{pr}{warn}{here}\n", e.branch));
    }
    out.push_str("┴\n");
    out.push_str(&format!("  {trunk} (trunk)\n"));
    if let Some(f) = fork_note {
        out.push_str(&format!("\nnote: `{f}` has multiple children; showing one line. Use `git stack status` from another branch to see the others.\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_prior_number_prefix() {
        assert_eq!(numbered_title("[2/9] Add widget", 0, 3), "[1/3] Add widget");
        assert_eq!(numbered_title("Add widget", 2, 3), "[3/3] Add widget");
        // Not a number prefix -> left intact.
        assert_eq!(numbered_title("[wip] thing", 0, 1), "[1/1] [wip] thing");
    }

    #[test]
    fn compose_replaces_existing_block() {
        let body = format!("Hello\n\n{BEGIN}\nold\n{END}\n");
        let composed = compose_body(&body, "new-nav");
        assert!(composed.contains("new-nav"));
        assert!(!composed.contains("old"));
        assert!(composed.starts_with("Hello"));
        // Only one block after recomposition.
        assert_eq!(composed.matches(BEGIN).count(), 1);
    }

    #[test]
    fn compose_into_empty_body() {
        let composed = compose_body("", "nav");
        assert_eq!(composed, format!("{BEGIN}\nnav\n{END}"));
    }
}
