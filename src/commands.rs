//! Implementations of each `git stack` subcommand.

use crate::render::{self, Entry, PrRef};
use crate::stack::Stack;
use crate::{gh, git, meta};
use anyhow::{bail, Result};

/// `git stack init [--trunk <branch>]`
pub fn init(trunk: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let trunk = match trunk {
        Some(t) => t,
        None => meta::trunk()?, // detect main/master
    };
    if !git::branch_exists(&trunk) {
        bail!("trunk branch `{trunk}` does not exist");
    }
    meta::set_trunk(&trunk)?;
    println!("Initialized git-stack. Trunk is `{trunk}`.");
    println!("Create your first stacked branch with:  git stack create <name>");
    Ok(())
}

/// `git stack create <name>` — new branch on top of the current one.
pub fn create(name: &str) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    if git::branch_exists(name) {
        bail!("branch `{name}` already exists");
    }
    let parent = git::current_branch()?;
    let parent_sha = git::rev_parse(&parent)?;

    git::create_branch(name, &parent)?;
    meta::set_parent(name, &parent)?;
    meta::set_parent_sha(name, &parent_sha)?;
    git::checkout(name)?;

    if parent == trunk {
        println!("Created `{name}` on trunk `{trunk}`. It is the bottom of a new stack.");
    } else {
        println!("Created `{name}` on top of `{parent}`.");
    }
    println!("Make your commits, then `git stack submit` to open PRs.");
    Ok(())
}

/// `git stack track [--parent <branch>]` — adopt the current branch.
pub fn track(parent: Option<String>) -> Result<()> {
    git::ensure_repo()?;
    let trunk = meta::trunk()?;
    let branch = git::current_branch()?;
    if branch == trunk {
        bail!("cannot track the trunk branch itself");
    }
    let parent = match parent {
        Some(p) => p,
        None => trunk.clone(),
    };
    if !git::branch_exists(&parent) {
        bail!("parent branch `{parent}` does not exist");
    }
    if !git::is_ancestor(&parent, &branch) {
        bail!(
            "`{parent}` is not an ancestor of `{branch}`; pass the correct --parent or rebase first"
        );
    }
    let base = git::merge_base(&parent, &branch)?;
    meta::set_parent(&branch, &parent)?;
    meta::set_parent_sha(&branch, &base)?;
    println!("Tracking `{branch}` with parent `{parent}`.");
    Ok(())
}

/// `git stack untrack` — forget the current branch's stack metadata.
pub fn untrack() -> Result<()> {
    git::ensure_repo()?;
    let branch = git::current_branch()?;
    meta::untrack(&branch);
    println!("Stopped tracking `{branch}`.");
    Ok(())
}

/// `git stack status`
pub fn status() -> Result<()> {
    git::ensure_repo()?;
    let stack = Stack::load()?;
    let current = git::current_branch()?;

    // Choose a line to show: the current branch's, or the first root's.
    let anchor = if stack.is_tracked(&current) {
        current.clone()
    } else if current == stack.trunk {
        match stack.roots().into_iter().next() {
            Some(r) => r,
            None => {
                println!("No stacks yet. Create one with `git stack create <name>`.");
                return Ok(());
            }
        }
    } else {
        println!("Branch `{current}` is not tracked. Adopt it with `git stack track`.");
        return Ok(());
    };

    let line = stack.line_through(&anchor)?;
    let entries = build_entries(&line.branches)?;
    let fork = line.fork_at.as_deref();
    print!(
        "{}",
        render::status_tree(&entries, &current, &stack.trunk, fork)
    );
    Ok(())
}

/// `git stack prev` / `down` — check out the parent branch.
pub fn prev() -> Result<()> {
    git::ensure_repo()?;
    let branch = git::current_branch()?;
    match meta::parent(&branch) {
        Some(p) => {
            git::checkout(&p)?;
            Ok(())
        }
        None => bail!("`{branch}` has no tracked parent"),
    }
}

/// `git stack next` / `up` — check out the child branch.
pub fn next() -> Result<()> {
    git::ensure_repo()?;
    let stack = Stack::load()?;
    let branch = git::current_branch()?;
    let kids = stack.children(&branch);
    match kids.len() {
        0 => bail!("`{branch}` is at the top of its stack (no children)"),
        1 => git::checkout(&kids[0]),
        _ => {
            let list = kids.join("\n  ");
            bail!("`{branch}` has multiple children; check one out directly:\n  {list}")
        }
    }
}

/// `git stack sync` — restack everything onto the latest trunk.
pub fn sync() -> Result<()> {
    git::ensure_repo()?;
    if git::rebase_in_progress() {
        bail!("a rebase is already in progress; finish it (`git rebase --continue`/`--abort`) then re-run `git stack sync`");
    }
    let stack = Stack::load()?;
    let remote = meta::remote();
    let original = git::current_branch()?;

    println!("Fetching `{remote}`...");
    if let Err(e) = git::fetch(&remote) {
        eprintln!(
            "warning: fetch failed, restacking on local `{}` instead: {e}",
            stack.trunk
        );
    }

    // New trunk tip: prefer the remote-tracking ref, else local trunk.
    let new_trunk_tip = match git::remote_trunk(&remote, &stack.trunk) {
        Some(r) => git::rev_parse(&r)?,
        None => git::rev_parse(&stack.trunk)?,
    };
    // Fast-forward the local trunk ref if it isn't checked out.
    if original != stack.trunk {
        let _ = git::force_ref(&stack.trunk, &new_trunk_tip);
    }

    for branch in stack.topo_order() {
        let parent = match stack.parent_of(&branch) {
            Some(p) => p.to_string(),
            None => continue,
        };
        let parent_tip = if parent == stack.trunk {
            new_trunk_tip.clone()
        } else {
            git::rev_parse(&parent)?
        };
        // Anchor: where this branch was last based. Fall back to merge-base.
        let anchor = meta::parent_sha(&branch)
            .unwrap_or_else(|| git::merge_base(&parent, &branch).unwrap_or_default());

        if anchor == parent_tip {
            continue; // already based on the current parent tip
        }
        println!("Restacking `{branch}` onto `{parent}`...");
        if let Err(e) = git::rebase_onto(&parent_tip, &anchor, &branch) {
            eprintln!("\nConflict while restacking `{branch}`.");
            eprintln!(
                "Resolve the conflict, run `git rebase --continue`, then re-run `git stack sync`."
            );
            return Err(e);
        }
        meta::set_parent_sha(&branch, &parent_tip)?;
    }

    git::checkout(&original)?;
    println!("Stack is up to date with `{}`.", stack.trunk);
    Ok(())
}

/// `git stack submit [--draft]` — push the current stack line and open/update
/// its numbered PRs.
pub fn submit(draft: bool) -> Result<()> {
    git::ensure_repo()?;
    if !gh::ready() {
        bail!("`gh` is not installed or not authenticated; run `gh auth login`");
    }
    let stack = Stack::load()?;
    let current = git::current_branch()?;
    if !stack.is_tracked(&current) {
        bail!("`{current}` is not tracked; run `git stack create` or `git stack track` first");
    }
    let line = stack.line_through(&current)?;
    if let Some(fork) = &line.fork_at {
        eprintln!("warning: `{fork}` has multiple children; submitting only this line.");
    }
    let branches = &line.branches;
    let total = branches.len();

    // Guard: don't open an empty PR.
    for (i, b) in branches.iter().enumerate() {
        let base = if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        };
        if git::ahead_count(&base, b)? == 0 {
            bail!("`{b}` has no commits beyond `{base}`; add a commit before submitting");
        }
    }

    // Pass 1: push every branch (bottom-first so bases exist), then
    // create-or-find its PR to learn the number.
    let remote = meta::remote();
    let mut prs: Vec<Option<PrRef>> = vec![None; total];
    for (i, b) in branches.iter().enumerate() {
        let base = if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        };
        println!("Pushing `{b}`...");
        git::push(&remote, b)?;

        let subject = git::tip_subject(b)?;
        let title = render::numbered_title(&subject, i, total);

        let number = match gh::find(b)? {
            Some(pr) => pr.number,
            None => {
                // Temporary body; the real nav block is written in pass 2.
                gh::create(b, &base, &title, "Opening…", draft)?
            }
        };
        meta::set_pr(b, number)?;
    }

    // Re-read PR metadata now that all exist (numbers, urls, states).
    for (i, b) in branches.iter().enumerate() {
        if let Some(pr) = gh::find(b)? {
            prs[i] = Some(PrRef {
                number: pr.number,
                url: pr.url,
                state: pr.state,
            });
        }
    }

    // Pass 2: write correct base, numbered title and shared nav block on each.
    let entries: Vec<Entry> = branches
        .iter()
        .enumerate()
        .map(|(i, b)| Entry {
            branch: b.clone(),
            pr: prs[i].clone(),
        })
        .collect();

    for (i, b) in branches.iter().enumerate() {
        let base = if i == 0 {
            stack.trunk.clone()
        } else {
            branches[i - 1].clone()
        };
        let number = match &prs[i] {
            Some(p) => p.number,
            None => continue,
        };
        let subject = git::tip_subject(b)?;
        let title = render::numbered_title(&subject, i, total);
        let nav = render::nav_block(&entries, b, &stack.trunk);
        let body = render::compose_body("", &nav);
        gh::edit(number, &base, &title, &body)?;
    }

    println!("\nSubmitted {total} PR(s):");
    for (i, b) in branches.iter().enumerate() {
        if let Some(p) = &prs[i] {
            println!("  [{}/{}] {}  {}", i + 1, total, b, p.url);
        }
    }
    Ok(())
}

/// Assemble render entries (branch + cached PR) for a bottom-first branch list.
fn build_entries(branches: &[String]) -> Result<Vec<Entry>> {
    let mut entries = Vec::with_capacity(branches.len());
    for b in branches {
        let pr = meta::pr(b).map(|number| PrRef {
            number,
            url: String::new(),
            state: "?".to_string(),
        });
        entries.push(Entry {
            branch: b.clone(),
            pr,
        });
    }
    Ok(entries)
}
