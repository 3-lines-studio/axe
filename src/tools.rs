use crate::machine::ShellOutput;
use crate::{Tool, ToolOutput, new_tool, new_tool_with_progress};
use serde::Deserialize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;

const MAX_OUTPUT: usize = 16 * 1024;
const DEFAULT_BASH_TIMEOUT: u64 = 120;
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

pub fn build_tools(dir: &str) -> Vec<Tool> {
    build_tools_for(dir, crate::web::Web::from_env())
}

pub fn build_tools_for(dir: &str, web: Option<crate::web::Web>) -> Vec<Tool> {
    let mut tools = build_tools_with(std::sync::Arc::new(crate::machine::Local::new(dir)), web);
    tools.extend(crate::skills::tools(std::path::Path::new(dir)));
    tools
}

pub fn build_tools_on(machine: std::sync::Arc<dyn crate::machine::Machine>) -> Vec<Tool> {
    build_tools_with(machine, crate::web::Web::from_env())
}

pub fn build_tools_with(
    machine: std::sync::Arc<dyn crate::machine::Machine>,
    web: Option<crate::web::Web>,
) -> Vec<Tool> {
    let mut tools = vec![
        read(machine.clone()),
        write(machine.clone()),
        edit(machine.clone()),
        bash(machine),
    ];
    if let Some(web) = web {
        tools.push(crate::web::web(web));
    }
    tools
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let code = c as u32;
            code == 0x09
                || code == 0x0a
                || code == 0x0d
                || !(code <= 0x1f || (0xfff9..=0xfffb).contains(&code))
        })
        .collect()
}

fn read_file_tail(path: &std::path::Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(MAX_OUTPUT as u64);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

static BASH_TAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const CHILD_SLOTS: usize = 32;
static CHILD_PGIDS: [std::sync::atomic::AtomicI32; CHILD_SLOTS] =
    [const { std::sync::atomic::AtomicI32::new(0) }; CHILD_SLOTS];

fn register_pgid(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    for slot in &CHILD_PGIDS {
        if slot
            .compare_exchange(
                0,
                pgid,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            return true;
        }
    }
    false
}

fn unregister_pgid(pgid: i32) {
    for slot in &CHILD_PGIDS {
        let _ = slot.compare_exchange(
            pgid,
            0,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

pub fn kill_children() {
    for slot in &CHILD_PGIDS {
        let pgid = slot.swap(0, std::sync::atomic::Ordering::AcqRel);
        if pgid > 0 {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

unsafe extern "C" fn sigint_reap_children(_: libc::c_int) {
    kill_children();
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

fn ensure_sigint_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_reap_children as unsafe extern "C" fn(libc::c_int) as usize,
        );
    });
}

struct PgidGuard(i32);

impl Drop for PgidGuard {
    fn drop(&mut self) {
        unregister_pgid(self.0);
    }
}

#[cfg(target_os = "linux")]
fn child_pidfd(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        None
    } else {
        Some(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(not(target_os = "linux"))]
fn child_pidfd(_: u32) -> Option<OwnedFd> {
    None
}

fn wait_for_child(pidfd: Option<&OwnedFd>, timeout: std::time::Duration) {
    let Some(pidfd) = pidfd else {
        std::thread::sleep(timeout.min(std::time::Duration::from_millis(1)));
        return;
    };
    let mut fd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe {
        libc::poll(&mut fd, 1, millis.max(1));
    }
}

fn reap(child: &mut std::process::Child, grace: std::time::Duration) {
    let until = std::time::Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                if std::time::Instant::now() >= until {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

enum ShellEnd {
    Exited(std::process::ExitStatus),
    TimedOut,
}

pub fn run_shell(
    dir: &str,
    command: &str,
    timeout: u64,
    progress: &mut dyn FnMut(&str),
) -> ShellOutput {
    let env: Vec<(String, String)> = std::env::vars().collect();
    run_shell_with_env(dir, command, timeout, &env, progress)
}

pub fn run_shell_with_env(
    dir: &str,
    command: &str,
    timeout: u64,
    env: &[(String, String)],
    progress: &mut dyn FnMut(&str),
) -> ShellOutput {
    let tag = BASH_TAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ensure_sigint_handler();
    let out_path = std::env::temp_dir().join(format!("axe-bash-{}-{tag}.out", std::process::id()));
    let out_file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&out_path)
    {
        Ok(file) => file,
        Err(e) => return ShellOutput::failed(format!("error: {e}")),
    };
    let err_file = match out_file.try_clone() {
        Ok(file) => file,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return ShellOutput::failed(format!("error: {e}"));
        }
    };
    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg(command);
    if !dir.is_empty() {
        cmd.current_dir(dir);
    }
    cmd.env_clear()
        .envs(env.iter().map(|(key, value)| (key, value)));
    cmd.stdout(std::process::Stdio::from(out_file));
    cmd.stderr(std::process::Stdio::from(err_file));
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return ShellOutput::failed(format!("error: {e}"));
        }
    };
    let pgid = child.id() as i32;
    if !register_pgid(pgid) {
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        reap(&mut child, KILL_GRACE);
        return ShellOutput::failed("error: too many live bash processes");
    }
    let _guard = PgidGuard(pgid);
    let pidfd = child_pidfd(child.id());
    let started = std::time::Instant::now();
    let timeout_dur = std::time::Duration::from_secs(timeout);
    let mut last_progress = std::time::Instant::now();
    let end = loop {
        match child.try_wait() {
            Ok(Some(status)) => break ShellEnd::Exited(status),
            Ok(None) => {
                if started.elapsed() >= timeout_dur {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    reap(&mut child, KILL_GRACE);
                    break ShellEnd::TimedOut;
                }
                if last_progress.elapsed() >= std::time::Duration::from_millis(100) {
                    last_progress = std::time::Instant::now();
                    let tail = read_file_tail(&out_path);
                    if !tail.is_empty() {
                        progress(&sanitize(&tail));
                    }
                }
                let progress_wait =
                    std::time::Duration::from_millis(100).saturating_sub(last_progress.elapsed());
                let timeout_wait = timeout_dur.saturating_sub(started.elapsed());
                wait_for_child(pidfd.as_ref(), progress_wait.min(timeout_wait));
            }
            Err(e) => {
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                reap(&mut child, KILL_GRACE);
                let _ = std::fs::remove_file(&out_path);
                return ShellOutput::failed(format!("error: {e}"));
            }
        }
    };
    let truncated = std::fs::metadata(&out_path)
        .map(|metadata| metadata.len() > MAX_OUTPUT as u64)
        .unwrap_or(false);
    let output = if truncated {
        read_file_tail(&out_path)
    } else {
        std::fs::read(&out_path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    };
    let mut display = sanitize(&output);
    if truncated {
        display.push_str(&format!(
            "\n\n[Output truncated to the last 16KB. Full output: {}]",
            out_path.display()
        ));
    } else {
        let _ = std::fs::remove_file(&out_path);
    }
    let failure = match end {
        ShellEnd::TimedOut => Some(format!("command timed out after {timeout} seconds")),
        ShellEnd::Exited(status) if !status.success() => Some(status_str(status)),
        ShellEnd::Exited(_) => None,
    };
    if let Some(failure) = &failure {
        if !display.is_empty() && !display.ends_with('\n') {
            display.push('\n');
        }
        display.push_str(&format!("error: {failure}"));
    }
    ShellOutput {
        text: display,
        failed: failure.is_some(),
    }
}

pub fn bash(machine: std::sync::Arc<dyn crate::machine::Machine>) -> Tool {
    let mut t = new_tool_with_progress(
        "bash",
        "Run a bash command in the working directory. Returns stdout and stderr, truncated to the last 16KB. Default timeout 120 seconds.",
        r#"{"type":"object","properties":{"command":{"type":"string","description":"bash command"},"timeout":{"type":"integer","description":"Timeout in seconds"}},"required":["command"]}"#,
        move |a: BashArgs, progress: &mut dyn FnMut(&str)| {
            if a.timeout == Some(0) {
                return ToolOutput::error(
                    "error: invalid timeout: must be a positive number of seconds",
                );
            }
            let timeout = a.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT);
            let output = machine.run(&a.command, timeout, progress);
            if output.failed {
                ToolOutput::error(output.text)
            } else {
                ToolOutput::text(output.text)
            }
        },
    );
    t.snippet = "Execute bash commands (ls, grep, find, etc.)".to_string();
    t
}

fn status_str(st: std::process::ExitStatus) -> String {
    match st.code() {
        Some(code) => format!("exit status {code}"),
        None => format!("signal: {st:?}"),
    }
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::{apply_edits, sanitize};
    use crate::machine::ShellOutput;

    #[test]
    fn the_child_environment_is_exactly_the_caller_one() {
        let _lock = BASH_TEST.lock().unwrap();
        let env = vec![
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("ALFRED_TEST".to_string(), "yes".to_string()),
        ];
        let out =
            super::run_shell_with_env("", "printf %s \"$ALFRED_TEST\"", 10, &env, &mut |_| {});
        assert_eq!(out.text, "yes");
        let out = super::run_shell_with_env("", "printf %s \"$HOME\"", 10, &env, &mut |_| {});
        assert_eq!(out.text, "");
    }

    #[test]
    fn sanitize_strips_control_characters() {
        assert_eq!(sanitize("a\x00b\x1bc\x7fd"), "abc\u{7f}d");
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "[31mred[0m");
        assert_eq!(sanitize("keep\tnewline\ncr\r"), "keep\tnewline\ncr\r");
        assert_eq!(sanitize("\u{fff9}fmt\u{fffb}"), "fmt");
        assert_eq!(sanitize("emoji 🙈 ok"), "emoji 🙈 ok");
    }

    #[test]
    fn edit_preserves_mixed_line_endings() {
        let body = "a\r\nb\nc\r\n";
        let edits = vec![super::EditArg {
            old_text: "a\r\n".into(),
            new_text: "A\r\n".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap().content;
        assert_eq!(out, "A\r\nb\nc\r\n");

        let edits = vec![super::EditArg {
            old_text: "b".into(),
            new_text: "B\nB2".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap().content;
        assert_eq!(out, "a\r\nB\nB2\nc\r\n");
    }

    #[test]
    fn edit_crlf_roundtrip() {
        let body = "one\r\ntwo\r\nthree\r\n";
        let edits = vec![super::EditArg {
            old_text: "two".into(),
            new_text: "TWO\nTWO2".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap().content;
        assert_eq!(out, "one\r\nTWO\r\nTWO2\r\nthree\r\n");
    }

    #[test]
    fn edit_multibyte_content() {
        let body = "héllo wörld 🙈\nsecond\n";
        let edits = vec![super::EditArg {
            old_text: "wörld".into(),
            new_text: "planet".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap().content;
        assert_eq!(out, "héllo planet 🙈\nsecond\n");
    }

    #[test]
    fn edit_rejects_invalid_replacements() {
        let edit = |old: &str, new: &str| super::EditArg {
            old_text: old.into(),
            new_text: new.into(),
        };
        let error = apply_edits("f", "x x", &[edit("x", "y")]).unwrap_err();
        assert!(error.contains("2 occurrences"), "{error}");
        let error = apply_edits("f", "abc", &[edit("abc", "x"), edit("bc", "y")]).unwrap_err();
        assert!(error.contains("overlap"), "{error}");
        let error = apply_edits("f", "x", &[edit("", "y")]).unwrap_err();
        assert!(error.contains("must not be empty"), "{error}");
        let error = apply_edits("f", "x", &[edit("x", "x")]).unwrap_err();
        assert!(error.contains("No changes made"), "{error}");
    }

    #[test]
    fn edit_preserves_bom() {
        let edits = [super::EditArg {
            old_text: "old".into(),
            new_text: "new".into(),
        }];
        assert_eq!(
            apply_edits("f", "\u{feff}old", &edits).unwrap().content,
            "\u{feff}new"
        );
    }

    #[test]
    fn edit_falls_back_to_fuzzy_match() {
        let edits = [super::EditArg {
            old_text: "let s = \"hi\";".into(),
            new_text: "let s = \"bye\";".into(),
        }];
        let out = apply_edits("f", "let s = \u{201c}hi\u{201d};\n", &edits)
            .unwrap()
            .content;
        assert_eq!(out, "let s = \"bye\";\n");

        let edits = [super::EditArg {
            old_text: "a b\nc\n".into(),
            new_text: "X\n".into(),
        }];
        let out = apply_edits("f", "a b   \nc\n", &edits).unwrap().content;
        assert_eq!(out, "X\n");
    }

    #[test]
    fn edit_fuzzy_applies_multiple_groups() {
        let text = "alpha\u{a0}one\nkeep\nbeta\u{a0}two\n";
        let edits = [
            super::EditArg {
                old_text: "alpha one".into(),
                new_text: "ALPHA".into(),
            },
            super::EditArg {
                old_text: "beta two".into(),
                new_text: "BETA".into(),
            },
        ];
        let applied = apply_edits("f.txt", text, &edits).unwrap();
        assert_eq!(applied.content, "ALPHA\nkeep\nBETA\n");
        assert!(
            applied.patch.contains("-alpha\u{a0}one"),
            "{}",
            applied.patch
        );
        assert!(applied.patch.contains("+ALPHA"), "{}", applied.patch);
    }

    #[test]
    fn edit_accepts_quirk_inputs() {
        let parse = |s: &str| serde_json::from_str::<super::EditArgs>(s).unwrap();
        assert_eq!(
            parse(r##"{"path":"f","edits":"[{\"oldText\":\"a\",\"newText\":\"b\"}]"}"##)
                .edits
                .len(),
            1
        );
        assert_eq!(
            parse(r##"{"path":"f","edits":{"oldText":"a","newText":"b"}}"##)
                .edits
                .len(),
            1
        );
        assert_eq!(
            parse(r##"{"path":"f","oldText":"a","newText":"b"}"##)
                .edits
                .len(),
            1
        );
    }

    #[test]
    fn edit_returns_unified_patch() {
        let edits = [super::EditArg {
            old_text: "b\n".into(),
            new_text: "B\n".into(),
        }];
        let patch = apply_edits("f.txt", "a\nb\nc\n", &edits).unwrap().patch;
        assert!(patch.contains("@@ -1,3 +1,3 @@"), "{patch}");
        assert!(patch.contains("-b"), "{patch}");
        assert!(patch.contains("+B"), "{patch}");
    }

    #[test]
    fn edit_patch_counts_a_final_line_without_newline() {
        let edits = [super::EditArg {
            old_text: "b".into(),
            new_text: "B".into(),
        }];
        let patch = apply_edits("f.txt", "a\nb", &edits).unwrap().patch;
        assert!(patch.contains("@@ -1,2 +1,2 @@"), "{patch}");
        assert!(patch.contains("-b\n+B"), "{patch}");
    }

    #[test]
    fn edit_patch_covers_multiple_groups() {
        let text = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        let edits = [
            super::EditArg {
                old_text: "b\n".into(),
                new_text: "B\n".into(),
            },
            super::EditArg {
                old_text: "k\n".into(),
                new_text: "K\n".into(),
            },
        ];
        let patch = apply_edits("f.txt", text, &edits).unwrap().patch;
        assert_eq!(
            patch,
            "--- f.txt\n+++ f.txt\n@@ -1,6 +1,6 @@\n a\n-b\n+B\n c\n d\n e\n f\n@@ -7,6 +7,6 @@\n g\n h\n i\n j\n-k\n+K\n l\n"
        );
    }

    #[test]
    fn edit_truncates_large_patch() {
        let old: String = (0..500).map(|i| format!("old line {i}\n")).collect();
        let new: String = (0..500).map(|i| format!("new line {i}\n")).collect();
        let edits = [super::EditArg {
            old_text: old.clone(),
            new_text: new,
        }];
        let patch = apply_edits("f", &old, &edits).unwrap().patch;
        assert!(patch.contains("lines omitted"), "{patch}");
        assert!(patch.lines().count() <= 82, "{}", patch.lines().count());
    }

    #[test]
    fn edit_fuzzy_folds_full_width() {
        let edits = [super::EditArg {
            old_text: "(1)".into(),
            new_text: "(2)".into(),
        }];
        let out = apply_edits("f", "let x = \u{ff08}1\u{ff09};\n", &edits)
            .unwrap()
            .content;
        assert_eq!(out, "let x = (2);\n");
    }

    fn local() -> std::sync::Arc<dyn crate::machine::Machine> {
        std::sync::Arc::new(crate::machine::Local::new(""))
    }

    #[test]
    fn read_accepts_float_offset() {
        let dir = std::env::temp_dir().join(format!("axe-read-float-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let read = crate::tools::read(local());
        let args = format!(
            r#"{{"path":"{}","offset":2.0,"limit":1.0}}"#,
            path.display()
        );
        let out = read.run_plain(&args, &mut |_| {}).text;
        assert!(out.starts_with("b"), "got: {out}");
        assert!(!out.contains("invalid arguments"), "got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attaches_images_and_rejects_binary() {
        let dir = std::env::temp_dir().join(format!("axe-read-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = crate::tools::read(local());

        let png = dir.join("shot.png");
        std::fs::write(
            &png,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .unwrap();
        let out = read.run_plain(&format!(r#"{{"path":"{}"}}"#, png.display()), &mut |_| {});
        assert_eq!(out.images.len(), 1, "text: {}", out.text);
        assert!(out.images[0].url.starts_with("data:image/png;base64,"));
        assert!(out.text.contains("shot.png"), "got: {}", out.text);

        let bin = dir.join("thing.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3, 0, 4]).unwrap();
        let out = read.run_plain(&format!(r#"{{"path":"{}"}}"#, bin.display()), &mut |_| {});
        assert!(out.images.is_empty());
        assert!(out.text.contains("binary file"), "got: {}", out.text);

        let txt = dir.join("t.txt");
        std::fs::write(&txt, "hello\n").unwrap();
        let out = read.run_plain(&format!(r#"{{"path":"{}"}}"#, txt.display()), &mut |_| {});
        assert!(out.images.is_empty());
        assert_eq!(out.text, "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kill_children_reaps_spawned_group() {
        use std::os::unix::process::CommandExt;
        let _lock = BASH_TEST.lock().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        assert!(super::register_pgid(pgid));
        crate::tools::kill_children();
        let st = child.wait().unwrap();
        assert!(!st.success());
    }

    static BASH_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reap_gives_up_on_live_child() {
        use std::os::unix::process::CommandExt;
        let _lock = BASH_TEST.lock().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        super::reap(&mut child, std::time::Duration::from_millis(200));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }

    #[test]
    fn a_failing_command_is_marked_as_an_error() {
        let _lock = BASH_TEST.lock().unwrap();
        let failed = super::run_shell("", "exit 3", 10, &mut |_| {});
        assert!(failed.failed);
        assert!(failed.text.ends_with("error: exit status 3"));
        let ok = super::run_shell("", "true", 10, &mut |_| {});
        assert!(!ok.failed);
    }

    #[test]
    fn bash_truncation_notice() {
        let _lock = BASH_TEST.lock().unwrap();
        let bash = crate::tools::bash(local());
        let args = serde_json::json!({"command": "yes | head -c 20000"}).to_string();
        let out = bash.run_plain(&args, &mut |_| {}).text;
        assert!(
            out.contains("Output truncated to the last 16KB"),
            "got tail: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(
            out.contains("Full output:"),
            "got: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(
            !out.contains("[truncated]"),
            "got: {}",
            &out[out.len().saturating_sub(200)..]
        );
        let path = out
            .split("Full output: ")
            .nth(1)
            .and_then(|s| s.trim().split(']').next())
            .unwrap();
        assert!(
            std::path::Path::new(path).exists(),
            "full output file missing: {path}"
        );
    }

    #[derive(Default)]
    struct Fake {
        files: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
        runs: std::sync::Mutex<Vec<String>>,
    }

    impl crate::machine::Machine for Fake {
        fn read(&self, path: &str) -> Result<Vec<u8>, String> {
            self.files
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or_else(|| format!("no such file: {path}"))
        }

        fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), bytes.to_vec());
            Ok(())
        }

        fn list(&self, _: &str) -> Result<Vec<crate::machine::Entry>, String> {
            Ok(Vec::new())
        }

        fn remove(&self, path: &str) -> Result<(), String> {
            self.files
                .lock()
                .unwrap()
                .remove(path)
                .map(|_| ())
                .ok_or_else(|| format!("no such file: {path}"))
        }

        fn run(&self, command: &str, _: u64, _: &mut dyn FnMut(&str)) -> ShellOutput {
            self.runs.lock().unwrap().push(command.to_string());
            ShellOutput::text(format!("ran: {command}"))
        }
    }

    #[test]
    fn tools_go_through_the_machine_they_are_given() {
        let fake = std::sync::Arc::new(Fake::default());
        let machine: std::sync::Arc<dyn crate::machine::Machine> = fake.clone();

        let write = crate::tools::write(machine.clone());
        let out = write
            .run_plain(
                r#"{"path":"/memoria/nota.txt","content":"hola\n"}"#,
                &mut |_| {},
            )
            .text;
        assert!(out.starts_with("wrote /memoria/nota.txt"), "got: {out}");

        let read = crate::tools::read(machine.clone());
        let out = read
            .run_plain(r#"{"path":"/memoria/nota.txt"}"#, &mut |_| {})
            .text;
        assert_eq!(out.trim(), "hola", "got: {out}");

        let edit = crate::tools::edit(machine.clone());
        let args = r#"{"path":"/memoria/nota.txt","edits":[{"oldText":"hola","newText":"chau"}]}"#;
        let out = edit.run_plain(args, &mut |_| {}).text;
        assert!(
            out.contains("Successfully replaced 1 block(s)"),
            "got: {out}"
        );
        let guardado = fake
            .files
            .lock()
            .unwrap()
            .get("/memoria/nota.txt")
            .cloned()
            .unwrap();
        assert_eq!(guardado, b"chau\n");

        let bash = crate::tools::bash(machine);
        let out = bash
            .run_plain(r#"{"command":"ls /memoria"}"#, &mut |_| {})
            .text;
        assert_eq!(out, "ran: ls /memoria");
        assert_eq!(fake.runs.lock().unwrap().as_slice(), ["ls /memoria"]);
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(crate) fn read_text_bytes(data: &[u8], offset: Option<usize>, limit: Option<usize>) -> String {
    read_text_reader(
        std::io::BufReader::new(std::io::Cursor::new(data)),
        offset,
        limit,
    )
}

pub(crate) fn read_text_reader<R: std::io::BufRead>(
    mut reader: R,
    offset: Option<usize>,
    limit: Option<usize>,
) -> String {
    let start = offset.unwrap_or(1).saturating_sub(1);
    let limit = limit.unwrap_or(usize::MAX);
    let mut line = Vec::new();
    let mut output = String::new();
    let mut total = 0usize;
    let mut skipped_partial = false;
    while total < start {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) => return format!("error: {e}"),
        };
        if buf.is_empty() {
            total += usize::from(skipped_partial);
            break;
        }
        let newlines = memchr::memchr_iter(b'\n', buf).count();
        if total + newlines < start {
            total += newlines;
            skipped_partial = !buf.ends_with(b"\n");
            let len = buf.len();
            reader.consume(len);
            continue;
        }
        let need = start - total;
        let at = memchr::memchr_iter(b'\n', buf)
            .nth(need - 1)
            .expect("enough newlines after the count");
        total = start;
        skipped_partial = false;
        reader.consume(at + 1);
    }
    let mut shown = 0usize;
    let mut overflow = false;
    let mut oversized = None;
    loop {
        if shown >= limit || overflow {
            let mut trailing = false;
            loop {
                let buf = match reader.fill_buf() {
                    Ok(buf) => buf,
                    Err(e) => return format!("error: {e}"),
                };
                if buf.is_empty() {
                    break;
                }
                total += memchr::memchr_iter(b'\n', buf).count();
                trailing = buf.last().is_some_and(|&byte| byte != b'\n');
                let len = buf.len();
                reader.consume(len);
            }
            total += usize::from(trailing);
            break;
        }
        line.clear();
        let read = match reader.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(e) => return format!("error: {e}"),
        };
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let index = total;
        total += 1;
        if index < start || index >= start.saturating_add(limit) || overflow {
            continue;
        }
        let text = sanitize(&String::from_utf8_lossy(&line));
        if shown == 0 && text.len() > MAX_OUTPUT {
            oversized = Some((index + 1, text.len()));
            overflow = true;
            continue;
        }
        let separator = usize::from(!output.is_empty());
        if output.len() + separator + text.len() > MAX_OUTPUT {
            overflow = true;
            continue;
        }
        if separator == 1 {
            output.push('\n');
        }
        output.push_str(&text);
        shown += 1;
    }
    if offset.is_some() && start >= total {
        return format!(
            "error: offset {} is beyond end of file ({} lines total)",
            offset.unwrap_or(1),
            total
        );
    }
    if let Some((line, bytes)) = oversized {
        return format!("[Line {line} is {bytes} bytes, exceeds the {MAX_OUTPUT} limit.]");
    }
    let end = start.saturating_add(shown).min(total);
    if overflow {
        return format!(
            "{}\n\n[Showing lines {}-{} of {} ({} limit). Use offset={} to continue.]",
            output,
            start + 1,
            end,
            total,
            MAX_OUTPUT,
            end + 1
        );
    }
    let remaining = total.saturating_sub(start.saturating_add(limit).min(total));
    if remaining > 0 {
        output.push_str(&format!(
            "\n\n[{remaining} more lines in file. Use offset={} to continue.]",
            start.saturating_add(limit) + 1
        ));
    }
    output
}

fn looks_binary(data: &[u8]) -> bool {
    data[..data.len().min(8192)].contains(&0)
}

pub fn read(machine: std::sync::Arc<dyn crate::machine::Machine>) -> Tool {
    let mut t = new_tool(
        "read",
        "Read a file. Output is truncated to 16KB; use offset and limit to continue. A JPEG, PNG, GIF or WebP image is attached so you can see it.",
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file"},"offset":{"type":"integer","description":"First line to read (1-indexed)"},"limit":{"type":"integer","description":"Maximum number of lines"}},"required":["path"]}"#,
        move |a: ReadArgs| {
            let head = match machine.read_head(&a.path, 8192) {
                Ok(head) => head,
                Err(e) => return ToolOutput::error(format!("error: {e}")),
            };
            if crate::image::looks_like_image(&head) {
                match machine.read(&a.path) {
                    Ok(data) => {
                        if let Some(image) = crate::image::attach_bytes(&a.path, &data) {
                            return ToolOutput {
                                text: format!("Attached {} for viewing.", a.path),
                                images: vec![image],
                                error: false,
                            };
                        }
                    }
                    Err(e) => return ToolOutput::error(format!("error: {e}")),
                }
            }
            if looks_binary(&head) {
                return ToolOutput::error(format!(
                    "error: {} is a binary file; read handles text files and JPEG, PNG, GIF, and WebP images",
                    a.path
                ));
            }
            match machine.read_text(&a.path, a.offset, a.limit) {
                Ok(text) => ToolOutput::text(text),
                Err(e) => ToolOutput::error(format!("error: {e}")),
            }
        },
    );
    t.snippet = "Read file contents (truncated, use offset to continue); images are attached so you can see them".to_string();
    t
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub fn write(machine: std::sync::Arc<dyn crate::machine::Machine>) -> Tool {
    let mut t = new_tool(
        "write",
        "Write content to a file. Creates missing parent directories and overwrites the file.",
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file"},"content":{"type":"string","description":"Content to write"}},"required":["path","content"]}"#,
        move |a: WriteArgs| match machine.write(&a.path, a.content.as_bytes()) {
            Ok(()) => format!("wrote {} ({} bytes)", a.path, a.content.len()),
            Err(e) => format!("error: {e}"),
        },
    );
    t.sequential = true;
    t.snippet = "Write content to a file".to_string();
    t
}

#[derive(Deserialize)]
struct EditArg {
    #[serde(rename = "oldText")]
    old_text: String,
    #[serde(rename = "newText")]
    new_text: String,
}

struct EditArgs {
    path: String,
    edits: Vec<EditArg>,
}

pub(crate) fn edit_count(arguments: &str) -> usize {
    serde_json::from_str::<EditArgs>(arguments).map_or(0, |arguments| arguments.edits.len())
}

impl<'de> Deserialize<'de> for EditArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("expected an object"))?;
        let path = obj
            .remove("path")
            .and_then(|v| v.as_str().map(str::to_string))
            .ok_or_else(|| D::Error::custom("missing path"))?;
        let mut edits: Vec<EditArg> = Vec::new();
        if let Some(value) = obj.remove("edits") {
            let parsed = match value {
                serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
                    .map_err(|e| D::Error::custom(format!("edits is not valid JSON: {e}")))?,
                other => other,
            };
            match parsed {
                serde_json::Value::Null => {}
                serde_json::Value::Array(items) => {
                    for item in items {
                        edits.push(serde_json::from_value(item).map_err(D::Error::custom)?);
                    }
                }
                serde_json::Value::Object(_) => {
                    edits.push(serde_json::from_value(parsed).map_err(D::Error::custom)?);
                }
                other => {
                    return Err(D::Error::custom(format!(
                        "edits must be an array, got {other}"
                    )));
                }
            }
        }
        if let (Some(old), Some(new)) = (obj.remove("oldText"), obj.remove("newText"))
            && let (Some(old), Some(new)) = (old.as_str(), new.as_str())
        {
            edits.push(EditArg {
                old_text: old.to_string(),
                new_text: new.to_string(),
            });
        }
        Ok(EditArgs { path, edits })
    }
}

fn normalize_lf(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\r') {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

fn lf_map(body: &str) -> Vec<usize> {
    let b = body.as_bytes();
    let mut map = Vec::with_capacity(b.len() + 1);
    let mut i = 0;
    while i < b.len() {
        map.push(i);
        if b[i] == b'\r' {
            i += if i + 1 < b.len() && b[i + 1] == b'\n' {
                2
            } else {
                1
            };
        } else {
            i += 1;
        }
    }
    map.push(b.len());
    map
}

fn with_ending(s: &str, ending: &str) -> String {
    if ending == "\r\n" {
        s.split('\n').collect::<Vec<_>>().join("\r\n")
    } else {
        s.to_string()
    }
}

fn region_ending(body: &str, bs: usize, be: usize) -> &'static str {
    if body[bs..be].contains("\r\n") || body[be..].starts_with("\r\n") {
        return "\r\n";
    }
    if body[be..].starts_with('\n') {
        return "\n";
    }
    if body[..bs].ends_with("\r\n") {
        return "\r\n";
    }
    if body[..bs].ends_with('\n') {
        return "\n";
    }
    if body.contains("\r\n") { "\r\n" } else { "\n" }
}

fn empty_old_error(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!("error: oldText must not be empty in {path}.")
    } else {
        format!("error: edits[{i}].oldText must not be empty in {path}.")
    }
}

fn not_found_error(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!(
            "error: Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "error: Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_error(path: &str, i: usize, total: usize, n: usize) -> String {
    if total == 1 {
        format!(
            "error: Found {n} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "error: Found {n} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn no_change_error(path: &str, total: usize) -> String {
    if total == 1 {
        format!("error: No changes made to {path}. The replacement produced identical content.")
    } else {
        format!("error: No changes made to {path}. The replacements produced identical content.")
    }
}

#[derive(Debug)]
struct Applied {
    content: String,
    patch: String,
}

struct Matched {
    edit: usize,
    start: usize,
    len: usize,
    new_text: String,
}

fn normalize_fuzzy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (index, line) in s.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let line = line.trim_end();
        if line.is_ascii() {
            out.push_str(line);
            continue;
        }
        for c in line.chars() {
            out.push(match c {
                '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
                '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
                '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
                _ => c,
            });
        }
    }
    out
}

fn split_lines(s: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = s.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

fn find_exact(
    path: &str,
    content: &str,
    olds: &[String],
    news: &[String],
) -> Result<Option<Vec<Matched>>, String> {
    let total = olds.len();
    let mut matched = Vec::with_capacity(total);
    let mut duplicate = None;
    for (i, old) in olds.iter().enumerate() {
        let Some(start) = content.find(old.as_str()) else {
            return Ok(None);
        };
        let remaining = &content[start + old.len()..];
        if duplicate.is_none() && remaining.contains(old.as_str()) {
            let n = 1 + remaining.matches(old.as_str()).count();
            duplicate = Some(duplicate_error(path, i, total, n));
        }
        matched.push(Matched {
            edit: i,
            start,
            len: old.len(),
            new_text: news[i].clone(),
        });
    }
    match duplicate {
        Some(error) => Err(error),
        None => Ok(Some(matched)),
    }
}

fn find_fuzzy(
    path: &str,
    base: &str,
    olds: &[String],
    news: &[String],
) -> Result<Vec<Matched>, String> {
    let total = olds.len();
    let mut matched = Vec::with_capacity(total);
    for (i, old) in olds.iter().enumerate() {
        let fuzzy_old = normalize_fuzzy(old);
        let Some(start) = base.find(fuzzy_old.as_str()) else {
            return Err(not_found_error(path, i, total));
        };
        let n = base.matches(fuzzy_old.as_str()).count();
        if n > 1 {
            return Err(duplicate_error(path, i, total, n));
        }
        matched.push(Matched {
            edit: i,
            start,
            len: fuzzy_old.len(),
            new_text: news[i].clone(),
        });
    }
    Ok(matched)
}

fn check_overlap(path: &str, matched: &mut [Matched]) -> Result<(), String> {
    matched.sort_by_key(|m| m.start);
    for w in matched.windows(2) {
        if w[0].start + w[0].len > w[1].start {
            return Err(format!(
                "error: edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                w[0].edit, w[1].edit
            ));
        }
    }
    Ok(())
}

struct ExactGroup {
    start: usize,
    end: usize,
    edits: Vec<usize>,
}

fn exact_group_regions(content: &str, matched: &[Matched]) -> Vec<ExactGroup> {
    let mut groups: Vec<ExactGroup> = Vec::new();
    for (i, m) in matched.iter().enumerate() {
        let start = content[..m.start].rfind('\n').map_or(0, |p| p + 1);
        let end = m.start + m.len;
        let end = if content.as_bytes()[end - 1] == b'\n' {
            end
        } else {
            content[end..]
                .find('\n')
                .map_or(content.len(), |p| end + p + 1)
        };
        if let Some(cur) = groups.last_mut()
            && start < cur.end
        {
            cur.end = cur.end.max(end);
            cur.edits.push(i);
            continue;
        }
        groups.push(ExactGroup {
            start,
            end,
            edits: vec![i],
        });
    }
    groups
}

fn exact_group_block(source: &str, group: &ExactGroup, matched: &[Matched]) -> String {
    let mut block = source[group.start..group.end].to_string();
    for &i in group.edits.iter().rev() {
        let m = &matched[i];
        let s = m.start - group.start;
        block.replace_range(s..s + m.len, &m.new_text);
    }
    block
}

struct FuzzyGroup {
    start: usize,
    end: usize,
    start_line: usize,
    end_line: usize,
    edits: Vec<usize>,
}

fn line_start(content: &str, pos: usize) -> usize {
    content[..pos].rfind('\n').map_or(0, |p| p + 1)
}

fn line_end(content: &str, end: usize) -> usize {
    if content.as_bytes()[end - 1] == b'\n' {
        end
    } else {
        content[end..]
            .find('\n')
            .map_or(content.len(), |p| end + p + 1)
    }
}

fn fuzzy_group_regions(content: &str, matched: &[Matched]) -> Vec<FuzzyGroup> {
    let mut groups: Vec<FuzzyGroup> = Vec::new();
    for (i, m) in matched.iter().enumerate() {
        let start = line_start(content, m.start);
        let end = line_end(content, m.start + m.len);
        if let Some(cur) = groups.last_mut()
            && start < cur.end
        {
            cur.end = cur.end.max(end);
            cur.edits.push(i);
            continue;
        }
        groups.push(FuzzyGroup {
            start,
            end,
            start_line: 0,
            end_line: 0,
            edits: vec![i],
        });
    }
    let mut line = 0;
    let mut cursor = 0;
    for group in &mut groups {
        line += line_count(&content[cursor..group.start]);
        cursor = group.start;
        group.start_line = line;
        group.end_line = line + line_count(&content[group.start..group.end]);
    }
    groups
}

fn line_offsets(text: &str, boundaries: &[usize]) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(boundaries.len());
    let mut line = 0;
    let mut pos = 0;
    for &target in boundaries {
        while line < target {
            let Some(next) = memchr::memchr(b'\n', &bytes[pos..]) else {
                pos = bytes.len();
                line = target;
                break;
            };
            pos += next + 1;
            line += 1;
        }
        out.push(pos);
    }
    out
}

struct FuzzyEdit {
    exact: Vec<ExactGroup>,
    blocks: Vec<String>,
    groups: Vec<FuzzyGroup>,
    boundaries: Vec<usize>,
    body_offsets: Vec<usize>,
}

fn fuzzy_groups(
    path: &str,
    normalized: &str,
    body: &str,
    base: &str,
    matched: &[Matched],
) -> Result<FuzzyEdit, String> {
    if line_count(body) != line_count(base) {
        return Err(format!(
            "error: cannot match edits in {path}: the file mixes line endings in a way that prevents safe reconstruction."
        ));
    }
    let groups = fuzzy_group_regions(base, matched);
    let mut boundaries: Vec<usize> = groups
        .iter()
        .flat_map(|group| [group.start_line, group.end_line])
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    let base_offsets = line_offsets(base, &boundaries);
    let body_offsets = line_offsets(body, &boundaries);
    let normalized_offsets = line_offsets(normalized, &boundaries);
    let at = |offsets: &[usize], line: usize| {
        offsets[boundaries
            .binary_search(&line)
            .expect("group boundary was indexed")]
    };
    let mut exact = Vec::with_capacity(groups.len());
    let mut blocks = Vec::with_capacity(groups.len());
    for group in &groups {
        let start = at(&base_offsets, group.start_line);
        let end = at(&base_offsets, group.end_line);
        let mut block = base[start..end].to_string();
        for &i in group.edits.iter().rev() {
            let m = &matched[i];
            let s = m.start - start;
            block.replace_range(s..s + m.len, &m.new_text);
        }
        blocks.push(block);
        exact.push(ExactGroup {
            start: at(&normalized_offsets, group.start_line),
            end: at(&normalized_offsets, group.end_line),
            edits: group.edits.clone(),
        });
    }
    Ok(FuzzyEdit {
        exact,
        blocks,
        groups,
        boundaries,
        body_offsets,
    })
}

fn apply_fuzzy(
    body: &str,
    groups: &[FuzzyGroup],
    blocks: &[String],
    boundaries: &[usize],
    body_offsets: &[usize],
) -> String {
    let mut out = String::new();
    let mut cursor = 0;
    for (index, group) in groups.iter().enumerate() {
        let at = |line: usize| {
            body_offsets[boundaries
                .binary_search(&line)
                .expect("group boundary was indexed")]
        };
        let bs = at(group.start_line);
        let be = at(group.end_line);
        out.push_str(&body[cursor..bs]);
        out.push_str(&with_ending(&blocks[index], region_ending(body, bs, be)));
        cursor = be;
    }
    out.push_str(&body[cursor..]);
    out
}

fn line_count(text: &str) -> usize {
    memchr::memchr_iter(b'\n', text.as_bytes()).count()
        + usize::from(!text.is_empty() && !text.ends_with('\n'))
}

fn lines_before(content: &str, mut pos: usize, count: usize) -> usize {
    for _ in 0..count {
        if pos == 0 {
            break;
        }
        let previous = content[..pos - 1].rfind('\n').map_or(0, |p| p + 1);
        pos = previous;
    }
    pos
}

fn lines_after(content: &str, mut pos: usize, count: usize) -> usize {
    for _ in 0..count {
        if pos >= content.len() {
            break;
        }
        let Some(next) = content[pos..].find('\n') else {
            return content.len();
        };
        pos += next + 1;
    }
    pos
}

fn exact_unified_patch(path: &str, old: &str, groups: &[ExactGroup], blocks: &[String]) -> String {
    const CONTEXT: usize = 4;
    let mut out = format!("--- {path}\n+++ {path}\n");
    let mut new_offset: isize = 0;
    let mut line_cursor = 0;
    let mut start_line = 0;
    for (index, group) in groups.iter().enumerate() {
        start_line += old[line_cursor..group.start]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        line_cursor = group.start;
        let mut old_start = lines_before(old, group.start, CONTEXT);
        if index > 0 {
            old_start = old_start.max(groups[index - 1].end);
        }
        let mut old_end = lines_after(old, group.end, CONTEXT);
        if let Some(next) = groups.get(index + 1) {
            old_end = old_end.min(next.start);
        }
        let before = line_count(&old[old_start..group.start]);
        let old_lines = line_count(&old[group.start..group.end]);
        let after = line_count(&old[group.end..old_end]);
        let old_start_line = start_line - before;
        let old_count = before + old_lines + after;
        let added = split_lines(&blocks[index]);
        let new_start = (old_start_line as isize + new_offset + 1).max(1);
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start_line + 1,
            old_count,
            new_start,
            before + added.len() + after
        ));
        for line in old[old_start..group.start].lines() {
            out.push_str(&format!(" {line}\n"));
        }
        for line in old[group.start..group.end].lines() {
            out.push_str(&format!("-{line}\n"));
        }
        for line in &added {
            out.push_str(&format!("+{line}\n"));
        }
        for line in old[group.end..old_end].lines() {
            out.push_str(&format!(" {line}\n"));
        }
        new_offset += added.len() as isize - old_lines as isize;
    }
    truncate_patch(out)
}

fn apply_exact(body: &str, matched: &[Matched]) -> String {
    let map = body.contains('\r').then(|| lf_map(body));
    let mut out = body.to_string();
    for m in matched.iter().rev() {
        let bs = map.as_ref().map_or(m.start, |map| map[m.start]);
        let be = map
            .as_ref()
            .map_or(m.start + m.len, |map| map[m.start + m.len]);
        let replacement = with_ending(&m.new_text, region_ending(body, bs, be));
        out.replace_range(bs..be, &replacement);
    }
    out
}

const MAX_PATCH_LINES: usize = 80;

fn truncate_patch(patch: String) -> String {
    let lines: Vec<&str> = patch.lines().collect();
    if lines.len() <= MAX_PATCH_LINES {
        return patch;
    }
    let head = MAX_PATCH_LINES / 2;
    let tail = MAX_PATCH_LINES - head;
    let omitted = lines.len() - head - tail;
    let mut out = String::new();
    for line in &lines[..head] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("... [{omitted} lines omitted] ...\n"));
    for line in &lines[lines.len() - tail..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn apply_edits(path: &str, content: &str, edits: &[EditArg]) -> Result<Applied, String> {
    if edits.is_empty() {
        return Err("error: edits must contain at least one replacement.".to_string());
    }
    let (bom, body) = match content.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", content),
    };
    let normalized = normalize_lf(body);
    let mut olds = Vec::with_capacity(edits.len());
    let mut news = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        if e.old_text.is_empty() {
            return Err(empty_old_error(path, i, edits.len()));
        }
        olds.push(normalize_lf(&e.old_text).into_owned());
        news.push(normalize_lf(&e.new_text).into_owned());
    }

    if let Some(mut matched) = find_exact(path, &normalized, &olds, &news)? {
        check_overlap(path, &mut matched)?;
        let groups = exact_group_regions(&normalized, &matched);
        let blocks: Vec<String> = groups
            .iter()
            .map(|g| exact_group_block(&normalized, g, &matched))
            .collect();
        let patch = exact_unified_patch(path, &normalized, &groups, &blocks);
        let out = apply_exact(body, &matched);
        if out == body {
            return Err(no_change_error(path, edits.len()));
        }
        return Ok(Applied {
            content: format!("{bom}{out}"),
            patch,
        });
    }

    let base = normalize_fuzzy(&normalized);
    let mut matched = find_fuzzy(path, &base, &olds, &news)?;
    check_overlap(path, &mut matched)?;
    let fuzzy = fuzzy_groups(path, &normalized, body, &base, &matched)?;
    let patch = exact_unified_patch(path, &normalized, &fuzzy.exact, &fuzzy.blocks);
    let out = apply_fuzzy(
        body,
        &fuzzy.groups,
        &fuzzy.blocks,
        &fuzzy.boundaries,
        &fuzzy.body_offsets,
    );
    if out == body {
        return Err(no_change_error(path, edits.len()));
    }
    Ok(Applied {
        content: format!("{bom}{out}"),
        patch,
    })
}

const EDIT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file"},"edits":{"type":"array","description":"Replacements applied to the original file, never to each other","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text to replace, unique in the file"},"newText":{"type":"string","description":"Replacement text"}},"required":["oldText","newText"]}}},"required":["path","edits"]}"#;

pub fn edit(machine: std::sync::Arc<dyn crate::machine::Machine>) -> Tool {
    let mut t = new_tool(
        "edit",
        "Edit one file with exact text replacements. Each oldText must match a unique, non-overlapping region of the original file. Merge nearby changes into one edit.",
        EDIT_SCHEMA,
        move |a: EditArgs| {
            if a.edits.is_empty() {
                return "error: edits must contain at least one replacement".into();
            }
            let data = match machine.read(&a.path) {
                Ok(data) => data,
                Err(e) => return format!("error: {e}"),
            };
            match String::from_utf8(data) {
                Err(e) => format!("error: {e}"),
                Ok(s) => match apply_edits(&a.path, &s, &a.edits) {
                    Ok(applied) => {
                        let n = a.edits.len();
                        match machine.write(&a.path, applied.content.as_bytes()) {
                            Ok(()) => format!(
                                "Successfully replaced {n} block(s) in {}.\n\n{}",
                                a.path, applied.patch
                            ),
                            Err(e) => format!("error: {e}"),
                        }
                    }
                    Err(e) => e,
                },
            }
        },
    );
    t.sequential = true;
    t.snippet = "Make precise file edits with exact text replacement, including multiple disjoint edits in one call".to_string();
    t
}
