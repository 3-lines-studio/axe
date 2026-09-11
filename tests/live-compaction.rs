use axe::{Message, OpenAI, Provider, Request, ToolCall, run, session, tui};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn message(role: &str, content: impl Into<String>) -> Message {
    Message {
        role: role.into(),
        content: content.into(),
        tool_calls: Vec::new(),
        tool_call_id: String::new(),
        reasoning: String::new(),
        images: Vec::new(),
    }
}

fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn required_facts() -> [(&'static str, &'static str); 9] {
    [
        ("codename", "ORBIT-CEDAR"),
        ("file", "src/relay.rs"),
        ("port", "4319"),
        ("forbidden", "tests/golden.txt"),
        ("next", "ACTION-RB17"),
        ("dependency", "DEPS-STDLIB-ONLY"),
        ("jitter", "full jitter"),
        ("rejected", "DNS caching"),
        ("test", "TEST-3-PASS"),
    ]
}

fn seed_entries() -> Vec<session::Entry> {
    let mut entries = vec![session::Entry::Message {
        message: message(
            "user",
            "Continue this task until it is complete. The project codename is ORBIT-CEDAR. Work in src/relay.rs. Use port 4319. Never modify tests/golden.txt. Use the standard library only (DEPS-STDLIB-ONLY). The next action is implement reconnect backoff (ACTION-RB17).",
        ),
    }];
    let mut call = message("assistant", "I will inspect the target file first.");
    call.tool_calls.push(ToolCall {
        id: "read-seed".into(),
        name: "read".into(),
        arguments: r#"{"path":"src/relay.rs"}"#.into(),
    });
    entries.push(session::Entry::Message { message: call });
    let mut result = message("tool", "fn relay() {}\n".repeat(1500));
    result.tool_call_id = "read-seed".into();
    entries.push(session::Entry::Message { message: result });
    let mut test_call = message("assistant", "I ran the relay tests.");
    test_call.tool_calls.push(ToolCall {
        id: "test-seed".into(),
        name: "bash".into(),
        arguments: r#"{"command":"test relay"}"#.into(),
    });
    entries.push(session::Entry::Message { message: test_call });
    let mut test_result = message("tool", "3 relay tests passed (TEST-3-PASS)");
    test_result.tool_call_id = "test-seed".into();
    entries.push(session::Entry::Message {
        message: test_result,
    });
    entries.push(session::Entry::Message {
        message: message(
            "assistant",
            "The target remains src/relay.rs. I have not changed tests/golden.txt. Reconnect backoff is still pending. We chose full jitter. DNS caching was rejected as unrelated.",
        ),
    });
    entries
}

fn add_cycle(entries: &mut Vec<session::Entry>, cycle: usize) {
    for turn in 0..8 {
        entries.push(session::Entry::Message {
            message: message(
                "user",
                format!("Cycle {cycle}, exploration note {turn}. Keep all active requirements unchanged."),
            ),
        });
        entries.push(session::Entry::Message {
            message: message(
                "assistant",
                format!(
                    "Checked unrelated option {turn}; it does not change the plan or requirements."
                ),
            ),
        });
    }
    entries.push(session::Entry::Message {
        message: message(
            "user",
            "Continue now. All prior requirements are still active and reconnect backoff remains the next action.",
        ),
    });
}

fn probe(provider: &OpenAI, model: &str, entries: &[session::Entry]) -> String {
    let mut messages = session::context_messages(entries);
    messages.push(message(
        "user",
        "Return the current values for codename, file, port, forbidden, next, dependency, jitter, rejected, and test. Preserve exact uppercase identifiers. Use one key=value line for each. Do not explain.",
    ));
    provider
        .complete(&Request {
            model,
            system: "Answer only from the supplied session context.",
            messages: &messages,
            tools: &[],
        })
        .expect("continuation probe")
        .message
        .content
}

#[test]
#[ignore]
fn live_model_continues_through_repeated_compaction() {
    let base =
        std::env::var("AXE_EVAL_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("AXE_EVAL_MODEL").unwrap_or_else(|_| "gpt-4.1-mini".into());
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let cycles = std::env::var("AXE_EVAL_COMPACTION_CYCLES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5);
    let provider = OpenAI::new(base, key);
    let mut entries = seed_entries();
    let mut failures = Vec::new();

    for cycle in 1..=cycles {
        add_cycle(&mut entries, cycle);
        let (summary, tokens_before, retained) =
            session::compact(&provider, &model, &entries).expect("compaction");
        entries.push(session::Entry::Compaction {
            summary,
            tokens_before,
            timestamp: cycle as i64,
            retained,
        });
        let context = session::context_messages(&entries);
        let context_chars: usize = context.iter().map(|message| message.content.len()).sum();
        let answer = probe(&provider, &model, &entries);
        let normalized_answer = normalized(&answer);
        let missing: Vec<&str> = required_facts()
            .iter()
            .filter_map(|(_, value)| {
                (!normalized_answer.contains(&normalized(value))).then_some(*value)
            })
            .collect();
        println!(
            "cycle={cycle} context_messages={} context_chars={context_chars} missing={} answer={answer:?}",
            context.len(),
            missing.join(",")
        );
        if !missing.is_empty() {
            failures.push(format!("cycle {cycle}: {}", missing.join(", ")));
        }
    }

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[derive(Default)]
struct ForcedCompactionSink {
    calls: BTreeMap<String, usize>,
    forced: usize,
}

impl run::Sink for ForcedCompactionSink {
    fn tool_start(&mut self, call: &ToolCall) {
        *self
            .calls
            .entry(format!("{}:{}", call.name, call.arguments))
            .or_default() += 1;
    }

    fn should_compact(&mut self, _input: usize, _output: usize) -> bool {
        if self.forced >= 6 {
            return false;
        }
        self.forced += 1;
        true
    }
}

fn append_run(entries: &mut Vec<session::Entry>, context_len: usize, messages: &[Message]) {
    for message in &messages[context_len..] {
        entries.push(session::Entry::Message {
            message: message.clone(),
        });
    }
}

#[test]
#[ignore]
fn live_agent_finishes_a_repository_task_across_forced_compactions() {
    let base =
        std::env::var("AXE_EVAL_BASE").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("AXE_EVAL_MODEL").unwrap_or_else(|_| "gpt-4.1-mini".into());
    let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
    let provider = OpenAI::new(base, key);
    let root = std::env::temp_dir().join(format!(
        "axe-live-compaction-{}-{}",
        std::process::id(),
        session::now_ms()
    ));
    std::fs::create_dir_all(&root).expect("create project");
    std::fs::write(
        root.join("relay.py"),
        "def retry_delays(attempts):\n    return [1] * attempts\n\ndef endpoint(host, port):\n    return f'{host}:{port}'\n",
    )
    .expect("relay.py");
    std::fs::write(
        root.join("test_relay.py"),
        "from relay import endpoint, retry_delays\n\nassert retry_delays(4) == [1, 2, 4, 8]\nassert endpoint('node', 4319) == 'node:4319'\n",
    )
    .expect("test_relay.py");
    std::fs::write(root.join("protected.txt"), "DO NOT CHANGE\n").expect("protected.txt");
    for index in 0..3 {
        std::fs::write(
            root.join(format!("notes-{index}.txt")),
            format!(
                "reference {index}\n{}",
                "irrelevant reference data\n".repeat(500)
            ),
        )
        .expect("notes");
    }

    let prompt = "Fix relay.py so test_relay.py passes. Do not modify test_relay.py or protected.txt. Use no dependencies. Inspect all three notes files before editing. Run the test after editing. When it passes, create report.txt containing exactly: ORBIT-CEDAR complete on port 4319 followed by a newline.";
    let mut entries = vec![session::Entry::Message {
        message: message("user", prompt),
    }];
    let tools = tui::build_tools(root.to_str().expect("project path"));
    let mut sink = ForcedCompactionSink::default();
    let mut compactions = 0;
    let mut restarts = 0;

    loop {
        let context = session::context_messages(&entries);
        let end = run::run_stream(
            &provider,
            &run::RunOptions {
                model: &model,
                system: "You are a coding agent. Complete the task with the available tools. Continue from compacted state without repeating finished work.",
                tools: &tools,
                max_turns: 20,
            },
            &context,
            &Arc::new(AtomicBool::new(false)),
            &mut sink,
        );
        append_run(&mut entries, context.len(), &end.messages);
        entries.push(session::Entry::Usage {
            input: end.usage.input,
            output: end.usage.output,
            cached_input: end.context.cached_input,
            context_input: end.context.input,
            context_output: end.context.output,
        });
        match end.outcome {
            run::Outcome::Done => break,
            run::Outcome::Compact => {
                restarts += 1;
                assert!(restarts <= 16, "forced continuation loop did not finish");
                let Ok((summary, tokens_before, retained)) =
                    session::compact(&provider, &model, &entries)
                else {
                    continue;
                };
                compactions += 1;
                entries.push(session::Entry::Compaction {
                    summary,
                    tokens_before,
                    timestamp: session::now_ms(),
                    retained,
                });
            }
            outcome => panic!("agent stopped: {outcome:?}"),
        }
    }

    let test = std::process::Command::new("python3")
        .arg("test_relay.py")
        .current_dir(&root)
        .output()
        .expect("run test");
    let report = std::fs::read_to_string(root.join("report.txt")).unwrap_or_default();
    let protected = std::fs::read_to_string(root.join("protected.txt")).unwrap_or_default();
    let repeated_calls: usize = sink
        .calls
        .values()
        .map(|count| count.saturating_sub(1))
        .sum();
    let repeated = sink
        .calls
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(call, count)| format!("{count}x {call}"))
        .collect::<Vec<_>>()
        .join(" | ");
    println!(
        "compactions={compactions} tool_calls={} repeated_calls={repeated_calls} repeated={repeated}",
        sink.calls.values().sum::<usize>()
    );
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        test.status.success(),
        "{}",
        String::from_utf8_lossy(&test.stderr)
    );
    assert_eq!(report, "ORBIT-CEDAR complete on port 4319\n");
    assert_eq!(protected, "DO NOT CHANGE\n");
    assert!(compactions >= 2, "task did not cross enough compactions");
    assert!(repeated_calls <= 3, "too many repeated tool calls");
}
