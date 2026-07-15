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

/// Build the shared stack-navigation block: a formatted, linked list of every
/// PR in the line in merge order (bottom first), with the current PR bolded and
/// marked. Each entry links to the PR's URL when known.
pub fn nav_block(line: &[Entry], current: &str, trunk: &str) -> String {
    let total = line.len();
    let mut lines = vec![
        format!(
            "### 📚 Stacked PR &nbsp;·&nbsp; {} of {}",
            position_of(line, current),
            total
        ),
        String::new(),
        "Merge in order, bottom to top:".to_string(),
        String::new(),
    ];
    // Bottom-first: index 0 merges first.
    for (i, e) in line.iter().enumerate() {
        let is_current = e.branch == current;
        let base = if i == 0 {
            trunk
        } else {
            line[i - 1].branch.as_str()
        };
        // Link text: `#<n> branch` linked to the PR URL if we have one.
        let label = match &e.pr {
            Some(p) if !p.url.is_empty() => format!("[#{} `{}`]({})", p.number, e.branch, p.url),
            Some(p) => format!("#{} `{}`", p.number, e.branch),
            None => format!("`{}` _(not submitted)_", e.branch),
        };
        let arrow = format!(" → `{base}`");
        let line_str = if is_current {
            format!("{}. **{label}{arrow}** &nbsp;👈 **this PR**", i + 1)
        } else {
            format!("{}. {label}{arrow}", i + 1)
        };
        lines.push(line_str);
    }
    lines.push(String::new());
    lines.push("<sub>🥞 Managed by git-stack — do not edit this list by hand.</sub>".to_string());
    lines.join("\n")
}

/// 1-based position of `current` within the (bottom-first) line.
fn position_of(line: &[Entry], current: &str) -> usize {
    line.iter()
        .position(|e| e.branch == current)
        .map_or(0, |i| i + 1)
}

/// Compose a PR body: the stack nav block PREPENDED, then the branch's
/// description below it. Any previous nav block (BEGIN..END) is stripped first,
/// so re-submitting is idempotent.
pub fn compose_body(description: &str, nav: &str) -> String {
    let desc = strip_block(description);
    let desc = desc.trim();
    if desc.is_empty() {
        format!("{BEGIN}\n{nav}\n{END}")
    } else {
        format!("{BEGIN}\n{nav}\n{END}\n\n---\n\n{desc}")
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
    fn compose_prepends_block_and_stays_idempotent() {
        // A prior render (block already at the top) plus the description below.
        let body = format!("{BEGIN}\nold\n{END}\n\n---\n\nHello");
        let composed = compose_body(&body, "new-nav");
        assert!(composed.starts_with(BEGIN), "nav block must be prepended");
        assert!(composed.contains("new-nav"));
        assert!(!composed.contains("old"), "old nav must be stripped");
        assert!(composed.contains("Hello"), "description preserved");
        // Exactly one block after recomposition (idempotent).
        assert_eq!(composed.matches(BEGIN).count(), 1);
    }

    #[test]
    fn compose_into_empty_description() {
        let composed = compose_body("", "nav");
        assert_eq!(composed, format!("{BEGIN}\nnav\n{END}"));
    }

    fn entry(branch: &str, number: u64, url: &str) -> Entry {
        Entry {
            branch: branch.to_string(),
            pr: Some(PrRef {
                number,
                url: url.to_string(),
                state: "OPEN".to_string(),
            }),
            conflicted: false,
        }
    }

    #[test]
    fn nav_block_links_and_marks_current() {
        let line = vec![
            entry("api", 10, "https://x/pull/10"),
            entry("service", 11, "https://x/pull/11"),
            entry("ui", 12, "https://x/pull/12"),
        ];
        let nav = nav_block(&line, "service", "main");
        // Bottom-first merge order, linked to PR URLs.
        assert!(nav.contains("1. [#10 `api`](https://x/pull/10) → `main`"));
        // Current PR is bolded and marked, and targets the branch below it.
        assert!(
            nav.contains("**[#11 `service`](https://x/pull/11) → `api`**"),
            "{nav}"
        );
        assert!(nav.contains("👈 **this PR**"));
        assert!(nav.contains("2 of 3"));
    }
}
