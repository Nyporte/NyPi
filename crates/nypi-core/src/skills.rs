//! Agent Skills (agentskills.io): directories containing `SKILL.md` with `name` and
//! `description` frontmatter. Only the index goes into the prompt; the model reads the file on demand.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    /// Hidden from the model's skill index; usable only through `/skill:name`.
    pub disable_model_invocation: bool,
}

/// Skill directories in priority order: the project's first (`None` when the folder is not
/// trusted, decision D61), then user-level, then Pi's.
pub fn default_skill_dirs(nypi_skills: &Path, project: Option<&Path>, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        project.map(|cwd| vec![cwd.join(".nypi/skills"), cwd.join(".agents/skills")]).unwrap_or_default();
    dirs.extend(extra.iter().cloned());
    dirs.push(nypi_skills.to_path_buf());
    dirs.extend(user_skill_dirs());
    dirs
}

/// The user's own skill folders outside NyPi's home (the shared `~/.agents/skills` and Pi's).
/// They load without a trust question, so the agent may not write to them (`Protected`).
pub fn user_skill_dirs() -> [PathBuf; 2] {
    let home = dirs::home_dir().unwrap_or_default();
    [home.join(".agents/skills"), home.join(".pi/agent/skills")]
}

/// Discover skills. Earlier directories win on name conflicts.
pub fn load_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut seen = HashSet::new();
    let mut skills = Vec::new();
    for dir in dirs {
        let mut found = Vec::new();
        collect(dir, 0, &mut found);
        found.sort_by(|a: &Skill, b| a.name.cmp(&b.name));
        for skill in found {
            if seen.insert(skill.name.clone()) {
                skills.push(skill);
            }
        }
    }
    skills
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<Skill>) {
    if depth > 4 {
        return;
    }
    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        if let Some(skill) = parse_skill(&skill_file) {
            out.push(skill);
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if path.is_dir() && !hidden && entry.file_name() != "node_modules" {
            collect(&path, depth + 1, out);
        }
    }
}

fn parse_skill(path: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(path).ok()?;
    let frontmatter = text.strip_prefix("---")?.split("\n---").next()?;
    let mut name = None;
    let mut description = None;
    let mut disable = false;
    let mut lines = frontmatter.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once(':') else { continue };
        let mut value = value.trim().to_string();
        // YAML block scalars (`description: >` or `|`) continue on indented lines.
        if value == ">" || value == "|" || value == ">-" || value == "|-" {
            let mut parts = Vec::new();
            while let Some(next) = lines.peek().filter(|l| l.starts_with(' ') || l.starts_with('\t')) {
                parts.push(next.trim().to_string());
                lines.next();
            }
            value = parts.join(" ");
        }
        let value = value.trim_matches('"').trim_matches('\'').to_string();
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "disable-model-invocation" => disable = value == "true",
            _ => {}
        }
    }
    let name = name.or_else(|| path.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))?;
    Some(Skill { name, description: description?, file_path: path.to_path_buf(), disable_model_invocation: disable })
}

/// The `<available_skills>` index for the system prompt (Pi's format).
pub fn format_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills.iter().filter(|s| !s.disable_model_invocation).collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "Resolve relative paths against the skill's directory (parent of SKILL.md). Use absolute paths in tool calls."
            .to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];
    for skill in visible {
        lines.push("  <skill>".into());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!("    <description>{}</description>", escape_xml(&skill.description)));
        lines.push(format!("    <location>{}</location>", escape_xml(&skill.file_path.display().to_string())));
        lines.push("  </skill>".into());
    }
    lines.push("</available_skills>".into());
    lines.join("\n")
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_skills_and_prefers_earlier_dirs() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a/pdf");
        let b = root.path().join("b/nested/pdf");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("SKILL.md"), "---\nname: pdf\ndescription: >\n  Read PDFs.\n  Use for PDFs.\n---\nbody")
            .unwrap();
        std::fs::write(b.join("SKILL.md"), "---\nname: pdf\ndescription: other\n---\n").unwrap();
        let skills = load_skills(&[root.path().join("a"), root.path().join("b")]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Read PDFs. Use for PDFs.");
        assert!(format_for_prompt(&skills).contains("<name>pdf</name>"));
    }
}
