//! `nypi reflections`: the reflection journal (nypi-core/src/reflection.rs), newest first, with
//! the newest shown in full. Skill proposals there are suggestions; nothing applies them.

use nypi_core::config::Paths;
use nypi_core::reflection::reflections_dir;

pub fn list(paths: &Paths, limit: usize) -> anyhow::Result<i32> {
    let dir = reflections_dir(paths);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "md")).collect())
        .unwrap_or_default();
    if files.is_empty() {
        nypi_hub::outln!("No reflections yet ({}). Lucia reflects after tasks that teach something.", dir.display());
        return Ok(0);
    }
    files.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    files.reverse();
    if let Some(newest) = files.first() {
        nypi_hub::outln!("{}", std::fs::read_to_string(newest).unwrap_or_default().trim_end());
    }
    if files.len() > 1 {
        nypi_hub::outln!("\nEarlier:");
        for file in files.iter().skip(1).take(limit.saturating_sub(1)) {
            let text = std::fs::read_to_string(file).unwrap_or_default();
            let lessons = text.lines().filter(|l| l.starts_with("- ") && !l.starts_with("- **")).count();
            let proposals = text.lines().filter(|l| l.starts_with("- **")).count();
            nypi_hub::outln!("  {}  {lessons} lesson(s), {proposals} proposal(s)", file.display());
        }
    }
    Ok(0)
}
