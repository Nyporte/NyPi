//! `nypi trust`: the user's decision on whether a folder's own NyPi configuration is loaded
//! (decision D61), and the question a terminal asks at startup when nobody has decided yet.
//! Only these two write the trust store; the agent's tools cannot.

use std::io::Write;
use std::path::Path;

use nypi_core::config::Paths;
use nypi_core::trust::{Trust, TrustStore};

const WHY: &str =
    "It can turn approvals off, add programs as tools, start MCP servers, and add specialists and skills.";

/// What applies to `cwd` and why.
pub fn status(paths: &Paths, cwd: &Path) -> anyhow::Result<i32> {
    let store = TrustStore::new(paths);
    let found = store.found(cwd);
    match found.is_empty() {
        true => println!("{} has no NyPi configuration of its own.", cwd.display()),
        false => println!("{} has its own NyPi configuration: {}.", cwd.display(), found.join(", ")),
    }
    match (store.decision(cwd), store.resolve(cwd)) {
        (Some((dir, true)), _) => println!("Trusted (decided for {}): it is loaded.", dir.display()),
        (Some((dir, false)), _) => println!("Not trusted (decided for {}): it is not loaded.", dir.display()),
        (None, Trust::Undecided { .. }) => {
            println!("Not decided: it is not loaded. {WHY}");
            println!("  nypi trust yes      load this folder's configuration");
            println!("  nypi trust parent   the same for everything under the parent folder");
            println!("  nypi trust no       never load it, and stop asking");
        }
        (None, _) => println!("Nothing to decide."),
    }
    Ok(0)
}

/// Record a decision for `cwd` (or its parent), or forget it.
pub fn decide(paths: &Paths, cwd: &Path, what: &str) -> anyhow::Result<i32> {
    let store = TrustStore::new(paths);
    match what {
        "yes" => {
            store.set(cwd, Some(true))?;
            println!("Trusted {}. Its NyPi configuration loads from the next start.", cwd.display());
        }
        "parent" => {
            let Some(parent) = cwd.parent() else { anyhow::bail!("{} has no parent folder", cwd.display()) };
            store.set(parent, Some(true))?;
            store.set(cwd, None)?;
            println!("Trusted {} and everything under it.", parent.display());
        }
        "no" => {
            store.set(cwd, Some(false))?;
            println!("{} is not trusted. Its NyPi configuration is never loaded.", cwd.display());
        }
        _ => {
            store.set(cwd, None)?;
            println!("Forgot the decision for {}.", cwd.display());
        }
    }
    Ok(0)
}

/// Ask once, at a terminal, before anything is loaded. No answer means not this time.
pub fn ask(paths: &Paths, cwd: &Path) -> anyhow::Result<()> {
    let store = TrustStore::new(paths);
    let Trust::Undecided { found } = store.resolve(cwd) else { return Ok(()) };
    println!("This folder has its own NyPi configuration: {}.", found.join(", "));
    println!("{WHY}");
    let parent = cwd.parent().map(|p| format!("  [p] yes, and everything under {}\n", p.display())).unwrap_or_default();
    print!(
        "Load it?\n  [y] yes, trust this folder\n{parent}  [n] no, and do not ask again\n  [Enter] not this time\n> "
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => decide(paths, cwd, "yes").map(|_| ()),
        "p" | "parent" => decide(paths, cwd, "parent").map(|_| ()),
        "n" | "no" => decide(paths, cwd, "no").map(|_| ()),
        _ => {
            println!("Not loaded this time. Decide later with: nypi trust yes");
            Ok(())
        }
    }
}
