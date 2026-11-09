use crate::{Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MAX_RESOURCES: usize = 50;
const MAX_RESOURCE_DEPTH: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
}

impl Skill {
    pub fn dir(&self) -> &Path {
        self.location.parent().unwrap_or(Path::new("."))
    }
}

pub fn discover(project: &Path) -> Vec<Skill> {
    merge(roots(project).into_iter().map(|root| scan(&root)))
}

fn merge(scopes: impl IntoIterator<Item = Vec<Skill>>) -> Vec<Skill> {
    let mut found: BTreeMap<String, Skill> = BTreeMap::new();
    for skill in scopes.into_iter().flatten() {
        found.insert(skill.name.clone(), skill);
    }
    found.into_values().collect()
}

fn roots(project: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config) = crate::config_dir() {
        roots.push(config.join("axe").join("skills"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".agents").join("skills"));
    }
    roots.push(project.join(".agents").join("skills"));
    roots.push(project.join(".axe").join("skills"));
    roots
}

fn scan(root: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let location = entry.path().join("SKILL.md");
        let Ok(text) = std::fs::read_to_string(&location) else {
            continue;
        };
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if let Some(skill) = parse(&dir_name, &text, location) {
            out.push(skill);
        }
    }
    out
}

fn parse(dir_name: &str, text: &str, location: PathBuf) -> Option<Skill> {
    let (front, _) = split_frontmatter(text)?;
    let description = field(front, "description")?;
    let name = field(front, "name").unwrap_or_else(|| dir_name.to_string());
    Some(Skill {
        name,
        description,
        location,
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let start = if rest.starts_with("\r\n") {
        2
    } else if rest.starts_with('\n') {
        1
    } else {
        return None;
    };
    let mut offset = start;
    for line in rest[start..].split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some((&rest[start..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn field(front: &str, key: &str) -> Option<String> {
    let mut lines = front.lines();
    while let Some(line) = lines.next() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let value = v.trim();
        if is_block(value) {
            let mut block = String::new();
            for line in lines.by_ref() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !line.starts_with([' ', '\t']) {
                    break;
                }
                if !block.is_empty() {
                    block.push(' ');
                }
                block.push_str(trimmed);
            }
            return Some(block).filter(|b| !b.is_empty());
        }
        return Some(unquote(value)).filter(|v| !v.is_empty());
    }
    None
}

fn is_block(value: &str) -> bool {
    matches!(value, "|" | ">" | "|-" | ">-" | "|+" | ">+")
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

pub fn tools(project: &Path) -> Vec<Tool> {
    from(&discover(project))
}

fn from(skills: &[Skill]) -> Vec<Tool> {
    if skills.is_empty() {
        return Vec::new();
    }
    vec![list(skills), load(skills)]
}

fn list(skills: &[Skill]) -> Tool {
    let mut text = String::new();
    for skill in skills {
        text.push_str(&format!("{}: {}\n", skill.name, skill.description));
    }
    let text = text.trim_end().to_string();
    let mut t = crate::new_tool(
        "list_skills",
        "List the installed Agent Skills with their names and descriptions. Call this when the task may match a skill, or when the user mentions a skill.",
        r#"{"type":"object","properties":{}}"#,
        move |_: NoArgs| text.clone(),
    );
    t.snippet = "List installed skills; load one with load_skill".to_string();
    t
}

#[derive(Deserialize)]
struct NoArgs {}

#[derive(Deserialize)]
struct LoadArgs {
    name: String,
}

fn load(skills: &[Skill]) -> Tool {
    let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    let parameters = serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "enum": names,
                "description": "Skill name, as listed by list_skills"
            }
        },
        "required": ["name"]
    });
    let registry = skills.to_vec();
    Tool {
        name: "load_skill".to_string(),
        description: "Load one skill's instructions into the conversation. The result holds the SKILL.md body, the skill directory, and its bundled files. Read bundled files with the read tool only when the instructions call for them.".to_string(),
        parameters,
        snippet: "Load a skill's instructions by name".to_string(),
        sequential: false,
        run: Box::new(move |raw, _context: &ToolContext, _progress| {
            let name = match serde_json::from_str::<LoadArgs>(raw) {
                Ok(args) => args.name,
                Err(e) => {
                    return Err(ToolError::InvalidArguments(format!("load_skill: {e}")));
                }
            };
            let Some(skill) = registry.iter().find(|s| s.name == name) else {
                return Ok(ToolOutput::error(format!(
                    "error: unknown skill {name}; call list_skills for the valid names"
                )));
            };
            match render(skill) {
                Ok(text) => Ok(ToolOutput::text(text)),
                Err(e) => Ok(ToolOutput::error(e)),
            }
        }),
    }
}

pub fn render(skill: &Skill) -> Result<String, String> {
    let raw = std::fs::read_to_string(&skill.location)
        .map_err(|e| format!("error: cannot read {}: {e}", skill.location.display()))?;
    let body = split_frontmatter(&raw)
        .map(|(_, body)| body.trim())
        .unwrap_or_else(|| raw.trim());
    let dir = skill.dir();
    let mut out = format!("<skill_content name=\"{}\">\n{body}\n\n", skill.name);
    out.push_str(&format!(
        "Skill directory: {}\nRelative paths in this skill resolve against the skill directory.\n",
        dir.display()
    ));
    let resources = resources(dir);
    if !resources.is_empty() {
        out.push_str("\n<skill_resources>\n");
        for resource in resources {
            out.push_str(&resource);
            out.push('\n');
        }
        out.push_str("</skill_resources>\n");
    }
    out.push_str("</skill_content>");
    Ok(out)
}

pub fn activated_name(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("<skill_content name=\"")?;
    let (name, _) = rest.split_once('"')?;
    (!name.is_empty()).then_some(name)
}

fn resources(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect(dir, dir, 0, &mut out);
    out.sort();
    out.truncate(MAX_RESOURCES);
    out
}

fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_RESOURCE_DEPTH || out.len() >= MAX_RESOURCES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, depth + 1, out);
            continue;
        }
        if name == "SKILL.md" {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path);
        out.push(relative.display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, text: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("axe-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_quoted_and_plain_fields() {
        let skill = parse(
            "pdf",
            "---\nname: pdf-processing\ndescription: \"Extract PDF text.\"\n---\nBody\n",
            PathBuf::from("/tmp/pdf/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "pdf-processing");
        assert_eq!(skill.description, "Extract PDF text.");
    }

    #[test]
    fn keeps_colons_inside_an_unquoted_description() {
        let skill = parse(
            "pdf",
            "---\nname: pdf\ndescription: Use when: the user asks about PDFs\n---\n",
            PathBuf::from("/tmp/pdf/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.description, "Use when: the user asks about PDFs");
    }

    #[test]
    fn reads_block_scalars() {
        let skill = parse(
            "pdf",
            "---\nname: pdf\ndescription: >\n  First line\n  second line\nlicense: MIT\n---\n",
            PathBuf::from("/tmp/pdf/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.description, "First line second line");
    }

    #[test]
    fn falls_back_to_the_directory_name() {
        let skill = parse(
            "pdf-processing",
            "---\ndescription: Handles PDFs.\n---\n",
            PathBuf::from("/tmp/pdf-processing/SKILL.md"),
        )
        .unwrap();
        assert_eq!(skill.name, "pdf-processing");
    }

    #[test]
    fn skips_skills_without_a_description() {
        assert!(
            parse(
                "pdf",
                "---\nname: pdf\n---\nBody\n",
                PathBuf::from("/tmp/pdf/SKILL.md")
            )
            .is_none()
        );
    }

    #[test]
    fn skips_files_without_frontmatter() {
        assert!(
            parse(
                "pdf",
                "# Just markdown\n",
                PathBuf::from("/tmp/pdf/SKILL.md")
            )
            .is_none()
        );
    }

    #[test]
    fn project_skills_override_user_skills() {
        let root = scratch("override");
        let user = root.join("home/.agents/skills/review");
        let project = root.join("project/.agents/skills/review");
        write(&user, "SKILL.md", "---\ndescription: user\n---\n");
        write(&project, "SKILL.md", "---\ndescription: project\n---\n");
        let merged = merge([
            scan(&root.join("home/.agents/skills")),
            scan(&root.join("project/.agents/skills")),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].description, "project");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ignores_directories_without_a_skill_file() {
        let root = scratch("layout");
        write(
            &root,
            "pdf/SKILL.md",
            "---\ndescription: Handles PDFs.\n---\n",
        );
        std::fs::create_dir_all(root.join("notes")).unwrap();
        std::fs::write(root.join("README.md"), "ignored").unwrap();
        let found = scan(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "pdf");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_strips_frontmatter_and_lists_resources() {
        let root = scratch("render");
        write(
            &root,
            "pdf/SKILL.md",
            "---\nname: pdf\ndescription: Handles PDFs.\n---\n# PDF\n\nRun scripts/extract.py\n",
        );
        write(&root, "pdf/scripts/extract.py", "print(1)\n");
        write(&root, "pdf/.hidden", "no\n");
        let skill = scan(&root).remove(0);
        let text = render(&skill).unwrap();
        assert!(text.starts_with("<skill_content name=\"pdf\">\n# PDF"));
        assert!(!text.contains("description: Handles PDFs."));
        assert!(text.contains("scripts/extract.py"));
        assert!(!text.contains(".hidden"));
        assert!(text.contains("Relative paths in this skill resolve against the skill directory."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_rejects_unknown_names() {
        let root = scratch("unknown");
        write(
            &root,
            "pdf/SKILL.md",
            "---\ndescription: Handles PDFs.\n---\n",
        );
        let tools = from(&scan(&root));
        let load = tools.iter().find(|t| t.name == "load_skill").unwrap();
        let output = load.run_plain(r#"{"name":"nope"}"#, &mut |_| {});
        assert!(output.error);
        assert!(output.text.contains("unknown skill"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rendered_skill_is_recognised_by_its_name() {
        let root = scratch("activated");
        write(
            &root,
            "pdf/SKILL.md",
            "---
name: pdf
description: Handles PDFs.
---
# PDF
",
        );
        let skill = scan(&root).remove(0);
        let text = render(&skill).unwrap();
        assert_eq!(activated_name(&text), Some("pdf"));
        assert_eq!(activated_name("hello"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn no_skills_means_no_tools() {
        let root = scratch("empty");
        assert!(from(&scan(&root)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
