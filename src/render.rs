//! Pure rendering helpers: the status tree, PR titles, and the shared queue
//! navigation block injected into every PR body. Kept side-effect-free so they
//! can be unit-tested without git or the network.

pub const BEGIN: &str = "<!-- git-queue:begin -->";
pub const END: &str = "<!-- git-queue:end -->";

/// One entry in a queue line for rendering purposes.
pub struct Entry {
    pub branch: String,
    pub pr: Option<PrRef>,
    /// The branch currently holds persisted conflict markers.
    pub conflicted: bool,
    /// Change-identity coverage: (commits with a Queued-Commit-Id, total commits).
    pub ids: Option<(usize, usize)>,
    /// Commits to render beneath the branch (newest first): `(Queued-Commit-Id?,
    /// subject)`. Empty for `status`; filled by `log`.
    pub commits: Vec<(Option<String>, String)>,
}

#[derive(Clone)]
pub struct PrRef {
    pub number: u64,
    pub url: String,
    pub state: String, // OPEN | CLOSED | MERGED
    /// APPROVED | CHANGES_REQUESTED | REVIEW_REQUIRED | None
    pub review: Option<String>,
}

/// The commit-status context the merge-order gate posts under.
pub const GATE_CONTEXT: &str = "git-queue/merge-order";

/// One planned merge-order status (the advisory "status gate"), to be posted
/// on the head commit of a PR.
#[derive(Debug, PartialEq)]
pub struct GateStatus {
    /// Head branch of the PR receiving the status.
    pub branch: String,
    /// true -> green ✓ (mergeable now); false -> red ✗ (out of order).
    pub success: bool,
    pub description: String,
    /// "Details" link for the status: the PR that must merge first.
    pub target_url: Option<String>,
}

/// Plan the advisory merge-order statuses for a queue line (bottom-first): the
/// bottom-most OPEN PR gets a success status, every open PR above it gets a
/// failure status naming the PR that must merge first. Merged/closed PRs and
/// branches without a PR get nothing.
pub fn gate_plan(entries: &[Entry]) -> Vec<GateStatus> {
    let mut bottom: Option<&PrRef> = None;
    let mut plan = Vec::new();
    for e in entries {
        let Some(pr) = &e.pr else { continue };
        if pr.state != "OPEN" {
            continue;
        }
        match bottom {
            None => {
                plan.push(GateStatus {
                    branch: e.branch.clone(),
                    success: true,
                    description: "Ready — front of the queue, merge this PR first".to_string(),
                    target_url: None,
                });
                bottom = Some(pr);
            }
            Some(b) => plan.push(GateStatus {
                branch: e.branch.clone(),
                success: false,
                description: format!("Do not merge — merge PR #{} first (queue order)", b.number),
                target_url: (!b.url.is_empty()).then(|| b.url.clone()),
            }),
        }
    }
    plan
}

/// Emoji for a PR's review decision.
fn approval_emoji(review: &Option<String>) -> &'static str {
    match review.as_deref() {
        Some("APPROVED") => "✅",
        Some("CHANGES_REQUESTED") => "♻️",
        _ => "⏳", // REVIEW_REQUIRED / not yet reviewed
    }
}

/// Emoji for a PR's merge state.
fn state_emoji(state: &str) -> &'static str {
    match state {
        "MERGED" => "🟣",
        "CLOSED" => "⚫",
        _ => "🟢", // OPEN
    }
}

/// Numbered title: `[k/n] <subject>`, stripping any prior `[i/j] ` prefix so
/// re-submitting doesn't pile up prefixes.
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

/// Build the shared queue-navigation block: a formatted, linked list of every
/// PR in the line in merge order (bottom first), with the current PR bolded and
/// marked. Each entry links to the PR's URL when known.
pub fn nav_block(line: &[Entry], current: &str, base: &str) -> String {
    let total = line.len();
    let mut lines = vec![
        format!(
            "### 📚 Queued PR &nbsp;·&nbsp; {} of {}",
            position_of(line, current),
            total
        ),
        String::new(),
        "Part of a queue. The PRs merge in FIFO order — the numbered order below, #1 \
         first. Merging one supersedes the PRs after it until the author runs \
         `git queue sync` (rebases the rest onto the merged base) and `git queue submit` \
         (retargets their PRs)."
            .to_string(),
        String::new(),
    ];
    // Bottom-first: index 0 merges first.
    for (i, e) in line.iter().enumerate() {
        let is_current = e.branch == current;
        let target = if i == 0 {
            base
        } else {
            line[i - 1].branch.as_str()
        };
        // Merge-state emoji always; approval emoji only while the PR is open
        // (a merged/closed PR's review status is no longer meaningful).
        let status = match &e.pr {
            Some(p) if p.state == "OPEN" => {
                format!("{}{} ", approval_emoji(&p.review), state_emoji(&p.state))
            }
            Some(p) => format!("{} ", state_emoji(&p.state)),
            None => String::new(),
        };
        // Link text: `#<n> branch` linked to the PR URL if we have one.
        let label = match &e.pr {
            Some(p) if !p.url.is_empty() => format!("[#{} `{}`]({})", p.number, e.branch, p.url),
            Some(p) => format!("#{} `{}`", p.number, e.branch),
            None => format!("`{}` _(not submitted)_", e.branch),
        };
        let arrow = format!(" → `{target}`");
        let line_str = if is_current {
            format!("{}. {status}**{label}{arrow}** &nbsp;👈 **this PR**", i + 1)
        } else {
            format!("{}. {status}{label}{arrow}", i + 1)
        };
        lines.push(line_str);
    }
    lines.push(String::new());
    lines.push(
        "<sub>✅ approved · ♻️ changes requested · ⏳ review pending &nbsp;|&nbsp; \
         🟣 merged · 🟢 open · ⚫ closed &nbsp;—&nbsp; status as of the last \
         `git queue submit`.</sub>"
            .to_string(),
    );
    lines.push("<sub>🥞 Managed by git-queue — do not edit this list by hand.</sub>".to_string());
    lines.join("\n")
}

/// 1-based position of `current` within the (bottom-first) line.
fn position_of(line: &[Entry], current: &str) -> usize {
    line.iter()
        .position(|e| e.branch == current)
        .map_or(0, |i| i + 1)
}

/// Compose a PR body: the queue nav block PREPENDED, then the branch's
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

/// Render the status tree, top of queue first, marking `current`.
/// `entries` is bottom-first; `base` is the branch the line merges into
/// (labelled "trunk" when it is the trunk, "base" otherwise).
pub fn status_tree(
    entries: &[Entry],
    current: &str,
    base: &str,
    base_is_trunk: bool,
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
        // Change-identity coverage: ✓ when every commit carries a Queued-Commit-Id,
        // a quiet fraction when only some do, and nothing at all when none do
        // (a queue that hasn't adopted ids yet isn't an anomaly).
        let ids = match e.ids {
            Some((h, t)) if t > 0 && h == t => "  id ✓".to_string(),
            Some((h, t)) if h > 0 => format!("  id {h}/{t}"),
            _ => String::new(),
        };
        out.push_str(&format!("{node} {}{pr}{ids}{warn}{here}\n", e.branch));
        for (id, subject) in &e.commits {
            // Abbreviate the id to `q-` + 8 chars; ULID time bits make that
            // distinct for commits made more than a moment apart.
            let abbrev = match id {
                Some(id) => id.chars().take(10).collect::<String>(),
                None => "(no id)".to_string(),
            };
            out.push_str(&format!("    {abbrev:<10}  {subject}\n"));
        }
    }
    out.push_str("┴\n");
    let label = if base_is_trunk { "trunk" } else { "base" };
    out.push_str(&format!("  {base} ({label})\n"));
    if let Some(f) = fork_note {
        out.push_str(&format!("\nnote: `{f}` has multiple children; showing one line. Use `git queue status` from another branch to see the others.\n"));
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

    fn entry(branch: &str, number: u64, url: &str, state: &str, review: Option<&str>) -> Entry {
        Entry {
            branch: branch.to_string(),
            pr: Some(PrRef {
                number,
                url: url.to_string(),
                state: state.to_string(),
                review: review.map(|s| s.to_string()),
            }),
            conflicted: false,
            ids: None,
            commits: Vec::new(),
        }
    }

    #[test]
    fn gate_plan_marks_bottom_ready_and_blocks_the_rest() {
        let line = vec![
            entry("api", 10, "https://x/pull/10", "OPEN", None),
            entry("service", 11, "https://x/pull/11", "OPEN", None),
            entry("ui", 12, "https://x/pull/12", "OPEN", None),
        ];
        let plan = gate_plan(&line);
        assert_eq!(plan.len(), 3);
        assert!(plan[0].success);
        assert_eq!(plan[0].branch, "api");
        assert_eq!(plan[0].target_url, None);
        for s in &plan[1..] {
            assert!(!s.success);
            assert!(s.description.contains("#10"), "{}", s.description);
            assert_eq!(s.target_url.as_deref(), Some("https://x/pull/10"));
        }
    }

    #[test]
    fn gate_plan_skips_merged_and_promotes_next_open_pr() {
        let line = vec![
            entry("api", 10, "https://x/pull/10", "MERGED", None),
            entry("service", 11, "https://x/pull/11", "OPEN", None),
            entry("ui", 12, "https://x/pull/12", "OPEN", None),
        ];
        let plan = gate_plan(&line);
        assert_eq!(plan.len(), 2);
        assert!(plan[0].success);
        assert_eq!(plan[0].branch, "service");
        assert!(!plan[1].success);
        assert!(
            plan[1].description.contains("#11"),
            "{}",
            plan[1].description
        );
    }

    #[test]
    fn gate_plan_ignores_closed_prs_and_unsubmitted_branches() {
        let mut line = vec![
            entry("api", 10, "https://x/pull/10", "CLOSED", None),
            entry("service", 11, "https://x/pull/11", "OPEN", None),
        ];
        line.push(Entry {
            branch: "ui".to_string(),
            pr: None,
            conflicted: false,
            ids: None,
            commits: Vec::new(),
        });
        let plan = gate_plan(&line);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].success);
        assert_eq!(plan[0].branch, "service");
    }

    #[test]
    fn gate_plan_is_empty_when_no_pr_is_open() {
        let line = vec![
            entry("api", 10, "https://x/pull/10", "MERGED", None),
            entry("service", 11, "https://x/pull/11", "CLOSED", None),
        ];
        assert!(gate_plan(&line).is_empty());
        assert!(gate_plan(&[]).is_empty());
    }

    #[test]
    fn gate_plan_descriptions_fit_github_status_limit() {
        // The GitHub statuses API caps descriptions at 140 characters.
        let line = vec![
            entry(
                "api",
                4_294_967_295,
                "https://x/pull/4294967295",
                "OPEN",
                None,
            ),
            entry("ui", 12, "https://x/pull/12", "OPEN", None),
        ];
        for s in gate_plan(&line) {
            assert!(s.description.len() <= 140, "{}", s.description);
        }
    }

    #[test]
    fn nav_block_links_marks_current_and_shows_status() {
        let line = vec![
            entry("api", 10, "https://x/pull/10", "MERGED", Some("APPROVED")),
            entry(
                "service",
                11,
                "https://x/pull/11",
                "OPEN",
                Some("CHANGES_REQUESTED"),
            ),
            entry("ui", 12, "https://x/pull/12", "OPEN", None),
        ];
        let nav = nav_block(&line, "service", "main");
        // Merged PR: state emoji only (approval no longer meaningful).
        assert!(
            nav.contains("1. 🟣 [#10 `api`](https://x/pull/10) → `main`"),
            "{nav}"
        );
        // Current PR: emojis, then bolded label targeting the branch below.
        assert!(
            nav.contains("2. ♻️🟢 **[#11 `service`](https://x/pull/11) → `api`**"),
            "{nav}"
        );
        // Not-yet-reviewed open PR.
        assert!(nav.contains("3. ⏳🟢 [#12 `ui`]"), "{nav}");
        assert!(nav.contains("👈 **this PR**"));
        assert!(nav.contains("2 of 3"));
        assert!(nav.contains("FIFO"), "merge order described as FIFO");
        assert!(
            !nav.contains("bottom-first"),
            "confusing bottom-first wording removed"
        );
    }
}
