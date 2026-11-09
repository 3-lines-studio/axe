use std::path::{Path, PathBuf};

fn project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("axe-skills-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn tool<'a>(tools: &'a [axe::Tool], name: &str) -> &'a axe::Tool {
    tools
        .iter()
        .find(|tool| tool.name == name)
        .unwrap_or_else(|| panic!("{name} is missing from {}", tools.len()))
}

#[test]
fn the_system_prompt_lists_the_skill_tools() {
    let root = project("prompt");
    write(
        &root,
        ".agents/skills/pdf/SKILL.md",
        "---\ndescription: Handles PDFs.\n---\n",
    );
    let tools = axe::tools::build_tools(root.to_str().unwrap());
    let prompt = axe::system_prompt(&tools);
    assert!(prompt.contains("- list_skills: "), "{prompt}");
    assert!(prompt.contains("- load_skill: "), "{prompt}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_then_load_returns_the_body_and_the_bundled_files() {
    let root = project("load");
    write(
        &root,
        ".agents/skills/pdf/SKILL.md",
        "---\nname: pdf\ndescription: Handles PDFs.\n---\n# PDF\n\nRun scripts/extract.py\n",
    );
    write(&root, ".agents/skills/pdf/scripts/extract.py", "print(1)\n");

    let tools = axe::tools::build_tools(root.to_str().unwrap());
    let listed = tool(&tools, "list_skills").run_plain("{}", &mut |_| {});
    assert!(
        listed.text.contains("pdf: Handles PDFs."),
        "{}",
        listed.text
    );

    let loaded = tool(&tools, "load_skill").run_plain(r#"{"name":"pdf"}"#, &mut |_| {});
    assert!(!loaded.error, "{}", loaded.text);
    assert!(
        loaded
            .text
            .starts_with("<skill_content name=\"pdf\">\n# PDF")
    );
    assert!(
        loaded.text.contains("scripts/extract.py"),
        "{}",
        loaded.text
    );
    assert!(!loaded.text.contains("description: Handles PDFs."));
    let _ = std::fs::remove_dir_all(&root);
}
