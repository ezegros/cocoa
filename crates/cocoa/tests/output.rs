//! Agent output presentation and copying through the public terminal view.

use std::error::Error;

use cocoa::chat::{Action, Chat};
use cocoa::theme::Theme;
use cocoa_core::{
    capability::{ActivityOutput, ActivityOutputKind, ActivitySummary},
    engine::{ActivityStatus, Event as EngineEvent, EventKind, Observation},
    store::{InvocationId, OperationId},
};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{CrosstermBackend, TestBackend},
    buffer::{Buffer, Cell},
    crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    layout::Rect,
    style::{Color, Modifier},
};

#[test]
fn every_alias_uses_the_theme_accent_without_distinct_colors() -> Result<(), Box<dyn Error>> {
    for (theme, accent) in [
        (Theme::DarkChocolate, Color::Rgb(223, 173, 118)),
        (Theme::CacaoGrove, Color::Rgb(217, 188, 131)),
        (Theme::MilkChocolate, Color::Rgb(120, 67, 35)),
        (Theme::TerminalNative, Color::Reset),
    ] {
        for alias in [
            "x-low", "low", "medium", "high", "x-high", "ultra", "reviewer", "builder",
        ] {
            let mut chat = Chat::new(128)?;
            chat.set_theme(theme);
            chat.set_context(".", alias);
            let label = format!(" {alias} ");
            let welcome = draw(&mut chat, 80, 32)?;
            assert!(
                cells(&welcome, &label)?
                    .iter()
                    .all(|cell| cell.fg == accent),
                "{theme:?}: {alias} label"
            );
            assert!(
                cells(&welcome, "cocoa")?
                    .iter()
                    .all(|cell| cell.fg == accent),
                "{theme:?}: {alias} wordmark"
            );
            let mark: Vec<_> = welcome
                .content
                .iter()
                .filter(|cell| matches!(cell.symbol(), "█" | "▀" | "▄"))
                .collect();
            assert!(!mark.is_empty());
            assert!(
                mark.iter().all(|cell| cell.fg == accent),
                "{theme:?}: {alias} pod"
            );
            chat.initial_prompt("check")?;
            let busy = draw(&mut chat, 80, 32)?;
            assert!(
                cells(&busy, "(o|o)")?.iter().all(|cell| cell.fg == accent),
                "{theme:?}: {alias} loader"
            );
            assert!(
                cells(&busy, &label)?.iter().all(|cell| cell.fg == accent),
                "{theme:?}: {alias} label while busy"
            );
        }
    }
    Ok(())
}

#[test]
fn changing_themes_recolors_cached_output_without_changing_text_or_draft()
-> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    chat.complete("# Result\n\nText and `inline`.\n\n```rust\nlet count = 7;\n```\n\n```diff\n--- a/value.rs\n+++ b/value.rs\n@@ -1 +1 @@\n-old\n+new\n```");
    for character in "next draft".chars() {
        chat.input(key(KeyCode::Char(character)));
    }
    let original = text(&draw(&mut chat, 80, 40)?);
    for (theme, background, foreground, accent, success, error) in [
        (
            Theme::DarkChocolate,
            Color::Rgb(16, 12, 10),
            Color::Rgb(241, 229, 213),
            Color::Rgb(223, 173, 118),
            Color::Rgb(185, 203, 151),
            Color::Rgb(242, 148, 137),
        ),
        (
            Theme::CacaoGrove,
            Color::Rgb(11, 16, 13),
            Color::Rgb(229, 234, 220),
            Color::Rgb(217, 188, 131),
            Color::Rgb(181, 212, 155),
            Color::Rgb(244, 158, 140),
        ),
        (
            Theme::MilkChocolate,
            Color::Rgb(245, 237, 223),
            Color::Rgb(56, 40, 31),
            Color::Rgb(120, 67, 35),
            Color::Rgb(54, 84, 44),
            Color::Rgb(146, 45, 40),
        ),
        (
            Theme::TerminalNative,
            Color::Reset,
            Color::Reset,
            Color::Reset,
            Color::Green,
            Color::Red,
        ),
        (
            Theme::DarkChocolate,
            Color::Rgb(16, 12, 10),
            Color::Rgb(241, 229, 213),
            Color::Rgb(223, 173, 118),
            Color::Rgb(185, 203, 151),
            Color::Rgb(242, 148, 137),
        ),
    ] {
        chat.set_theme(theme);
        let view = draw(&mut chat, 80, 40)?;
        assert_eq!(text(&view), original, "{theme:?}");
        assert_eq!(view[(0, 0)].bg, background, "blank cells must be themed");
        assert_eq!(cells(&view, "Result")?[0].fg, foreground);
        assert_eq!(
            cells(&view, "inline")?[0].fg,
            accent,
            "cached Markdown must recolor"
        );
        assert_eq!(cells(&view, "-old")?[0].fg, error);
        assert_eq!(cells(&view, "+new")?[0].fg, success);
        assert_eq!(cells(&view, "next draft")?[1].fg, foreground);
        if theme == Theme::TerminalNative {
            assert!(
                cells(&view, "let count = 7;")?
                    .iter()
                    .all(|cell| cell.fg == Color::Reset)
            );
        } else {
            let code = cells(&view, "let count = 7;")?;
            assert_ne!(code[0].fg, code[4].fg);
        }
    }
    assert_eq!(
        chat.input(key(KeyCode::Enter)),
        Action::Submit("next draft".into())
    );
    Ok(())
}

#[test]
fn terminal_hyperlinks_preserve_wrapped_urls_and_plain_copy() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    chat.complete("See [docs](https://docs.example.test/a_(b)).\nBare HTTP://localhost:8123/long-path-that-wraps?next=hTtPs://nested.test&c=2,\nUnicode https://例え.test/道?q=λ.\n`https://code.example.test`");
    for character in "https://draft.example.test".chars() {
        chat.input(key(KeyCode::Char(character)));
    }
    let plain = draw(&mut chat, 36, 32)?;
    let output = tempfile::NamedTempFile::new()?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.reopen()?),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 36, 32)),
        },
    )?;
    terminal.draw(|frame| chat.render_terminal(frame))?;
    let first = std::fs::read(output.path())?;
    let links = linked_runs(&first)?;
    let expected = [
        "https://docs.example.test/a_(b)",
        "HTTP://localhost:8123/long-path-that-wraps?next=hTtPs://nested.test&c=2",
        "https://例え.test/道?q=λ",
        "https://code.example.test",
    ];
    for destination in expected {
        assert_eq!(
            links
                .iter()
                .filter(|(url, _)| url == destination)
                .map(|(_, fragment)| fragment.as_str())
                .collect::<String>(),
            destination,
            "every wrapped fragment must retain its full destination"
        );
    }
    assert!(
        links
            .iter()
            .all(|(url, _)| expected.contains(&url.as_str()))
    );
    assert!(
        links.len() > expected.len(),
        "the fixture must exercise wrapping"
    );
    terminal.draw(|frame| chat.render_terminal(frame))?;
    let second = std::fs::read(output.path())?;
    assert!(linked_runs(&second[first.len()..])?.is_empty());

    let value = text(&plain);
    let (row, line) = value
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("https://code.example.test"))
        .ok_or("missing code URL")?;
    let column = line
        .find("https://code.example.test")
        .ok_or("missing URL")? as u16;
    chat.input(mouse(
        MouseEventKind::Down(MouseButton::Left),
        column,
        row as u16,
    ));
    assert_eq!(
        chat.input(mouse(
            MouseEventKind::Up(MouseButton::Left),
            column + "https://code.example.test".len() as u16 - 1,
            row as u16,
        )),
        Action::Copy("https://code.example.test".into())
    );
    terminal.draw(|frame| chat.render_terminal(frame))?;
    assert_eq!(
        linked_runs(&std::fs::read(output.path())?[second.len()..])?
            .iter()
            .filter(|(url, _)| url == "https://code.example.test")
            .map(|(_, fragment)| fragment.as_str())
            .collect::<String>(),
        "https://code.example.test"
    );
    Ok(())
}

#[test]
fn terminal_hyperlinks_bound_destinations_and_leave_other_schemes_literal()
-> Result<(), Box<dyn Error>> {
    let accepted = format!("https://limit.test/{}", "a".repeat(2048 - 19));
    let rejected = format!("{accepted}b");
    assert_eq!(accepted.len(), 2048);
    let source = format!(
        "{accepted}.\n{rejected}\nhttp:// https://[invalid] javascript:alert(1) file:///tmp/note embeddedhttps://hidden.test"
    );
    let mut chat = Chat::new(8192)?;
    for character in source.chars() {
        chat.input(if character == '\n' {
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
        } else {
            key(KeyCode::Char(character))
        });
    }
    assert_eq!(
        chat.input(key(KeyCode::Enter)),
        Action::Submit(source.into())
    );
    chat.complete("");
    let output = tempfile::NamedTempFile::new()?;
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(output.reopen()?),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 88, 80)),
        },
    )?;
    terminal.draw(|frame| chat.render_terminal(frame))?;
    let links = linked_runs(&std::fs::read(output.path())?)?;
    assert!(!links.is_empty());
    assert!(links.iter().all(|(url, _)| url == &accepted));
    assert_eq!(
        links
            .iter()
            .map(|(_, fragment)| fragment.as_str())
            .collect::<String>(),
        accepted
    );
    Ok(())
}

#[test]
fn markdown_code_and_patch_styles_preserve_content() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    chat.complete("# Result\n\n**Strong** and `inline`.\n\n```rust\nlet count = 7;\n```\n\n```diff\n--- a/value.rs\n+++ b/value.rs\n@@ -1 +1 @@\n-let count = 7;\n+let count = 23;\n```\n\nEnd.");
    let view = draw(&mut chat, 80, 40)?;
    assert!(cells(&view, "Result")?[0].modifier.contains(Modifier::BOLD));
    assert!(cells(&view, "Strong")?[0].modifier.contains(Modifier::BOLD));
    assert!(cells(&view, "inline")?[0].fg != cells(&view, " and ")?[1].fg);
    let code = cells(&view, "let count = 7;")?;
    assert_ne!(code[0].fg, code[4].fg, "keywords must be highlighted");
    assert_ne!(code[12].fg, code[4].fg, "numbers must be highlighted");
    let removed = cells(&view, "-let count = 7;")?;
    let added = cells(&view, "+let count = 23;")?;
    assert_ne!(removed[0].fg, added[0].fg);
    assert_ne!(removed[13].bg, removed[2].bg);
    assert_ne!(added[13].bg, added[2].bg);
    assert!(cells(&view, "End.").is_ok());
    assert!(!text(&view).contains("```"));
    assert!(!text(&view).contains("# Result"));
    Ok(())
}

#[test]
fn compact_activities_keep_inline_counts_colors_and_command_tense() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    for (operation, title, summary, status) in [
        (
            1,
            "Read note.txt",
            Some(ActivitySummary::LineRange { start: 2, end: 3 }),
            Some(ActivityStatus::Completed),
        ),
        (
            2,
            "Write note.txt",
            Some(ActivitySummary::LineChanges {
                added: 32,
                removed: 0,
            }),
            Some(ActivityStatus::Completed),
        ),
        (
            3,
            "Edit note.txt",
            Some(ActivitySummary::LineChanges {
                added: 20,
                removed: 32,
            }),
            Some(ActivityStatus::Completed),
        ),
        (
            4,
            "Running cargo fmt --check",
            None,
            Some(ActivityStatus::Completed),
        ),
        (
            5,
            "Running cargo test",
            None,
            Some(ActivityStatus::Failed {
                exit_code: Some(101),
            }),
        ),
        (6, "Running cargo clippy", None, None),
        (
            7,
            "Running kill -TERM $$",
            None,
            Some(ActivityStatus::Signalled {
                signal: rustix::process::Signal::TERM.as_raw(),
            }),
        ),
        (8, "Running sleep 20", None, Some(ActivityStatus::TimedOut)),
    ] {
        emit(
            &mut chat,
            EventKind::CapabilityInput {
                operation: OperationId(operation),
                title: title.into(),
                text: "Safe input details.".into(),
                summary,
                truncated: false,
            },
        );
        if matches!(operation, 2 | 3 | 5) {
            emit(
                &mut chat,
                EventKind::CapabilityOutput {
                    operation: OperationId(operation),
                    output: ActivityOutput {
                        text: if operation == 5 {
                            "error: test failed."
                        } else {
                            "@@ -1 +1 @@\n-old\n+new\n"
                        }
                        .into(),
                        kind: if operation == 5 {
                            ActivityOutputKind::Command
                        } else {
                            ActivityOutputKind::Diff
                        },
                        summary,
                        truncated: false,
                    },
                },
            );
        }
        if let Some(status) = status {
            emit(
                &mut chat,
                EventKind::CapabilityFinished {
                    operation: OperationId(operation),
                    status,
                },
            );
        }
    }
    let view = draw(&mut chat, 64, 24)?;
    let plain = text(&view);
    let first = plain
        .lines()
        .position(|line| line.contains("Read note.txt"))
        .ok_or("missing read")?;
    let expected = [
        "Read note.txt L2-L3",
        "Wrote note.txt +32 ▸",
        "Edited note.txt +20 −32 ▸",
        "Ran cargo fmt --check",
        "Ran cargo test · exit 101 ▸",
        "Running cargo clippy",
        "Ran kill -TERM $$ · SIGTERM",
        "Ran sleep 20 · timed out",
    ];
    assert_eq!(
        plain
            .lines()
            .skip(first)
            .take(8)
            .map(str::trim)
            .collect::<Vec<_>>(),
        expected
    );
    let added = cells(&view, "+20")?[0].fg;
    let removed = cells(&view, "−32")?[0].fg;
    assert!(matches!(added, Color::Rgb(red, green, blue) if green > red && green > blue));
    assert!(matches!(removed, Color::Rgb(red, green, blue) if red > green && red > blue));
    assert_eq!(cells(&view, "+32")?[0].fg, added);
    assert_eq!(cells(&view, "Ran cargo test")?[0].fg, removed);
    assert_eq!(cells(&view, "exit 101")?[0].fg, removed);
    assert_eq!(cells(&view, "SIGTERM")?[0].fg, removed);
    assert_eq!(cells(&view, "timed out")?[0].fg, removed);
    assert_ne!(cells(&view, "Ran cargo fmt")?[0].fg, removed);
    assert!(!plain.contains("Failed"));
    assert!(!plain.contains("Thought"));
    assert!(!plain.contains("Safe input"));
    chat.input(key(KeyCode::Tab));
    chat.input(key(KeyCode::Right));
    let selected = draw(&mut chat, 64, 24)?;
    assert_eq!(cells(&selected, "Ran cargo test")?[0].fg, removed);
    assert!(text(&selected).contains("error: test failed."));
    assert!(!text(&selected).contains("Safe input details."));
    emit(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(3),
            title: "Edit crates/cocoa-tools/tests/fixtures/notes/project-handoff-note.txt".into(),
            text: "Replacements requested: 1".into(),
            summary: Some(ActivitySummary::LineChanges {
                added: 20,
                removed: 32,
            }),
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityOutput {
            operation: OperationId(3),
            output: ActivityOutput {
                text: "@@ -1 +1 @@\n-old\n+new\n".into(),
                kind: ActivityOutputKind::Diff,
                summary: None,
                truncated: false,
            },
        },
    );
    let narrow = draw(&mut chat, 24, 24)?;
    let narrow = text(&narrow);
    let edited = narrow
        .lines()
        .find(|line| line.contains("+20 −32 ▸"))
        .ok_or("missing edit counts")?;
    assert!(edited.trim_start().starts_with("Edit"), "{narrow}");
    assert!(edited.contains('…'), "{narrow}");
    assert!(!narrow.contains("project-handoff-note.txt"), "{narrow}");
    assert!(narrow.contains("L2-L3"));
    assert!(narrow.contains("exit 101 ▾"));
    assert!(narrow.contains("SIGTERM"));
    assert!(narrow.contains("timed out"));
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(6),
            status: ActivityStatus::Completed,
        },
    );
    chat.complete("Finished.");
    click_activity(&mut chat, "Work Done")?;
    let completed = draw(&mut chat, 64, 24)?;
    assert!(text(&completed).contains("Ran cargo clippy"));
    assert_eq!(cells(&completed, "Ran cargo test")?[0].fg, removed);
    assert!(!text(&completed).contains("Running cargo clippy"));
    Ok(())
}

#[test]
fn command_rows_settle_after_answers_and_during_later_invocations() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    for (operation, title) in [(1, "Running idle-command"), (2, "Running later-command")] {
        emit(
            &mut chat,
            EventKind::CapabilityInput {
                operation: OperationId(operation),
                title: title.into(),
                text: String::new(),
                summary: None,
                truncated: false,
            },
        );
        emit(
            &mut chat,
            EventKind::CapabilityFinished {
                operation: OperationId(operation),
                status: ActivityStatus::Running,
            },
        );
    }
    chat.complete("First answer.");
    let idle = text(&draw(&mut chat, 64, 24)?);
    assert!(idle.contains("Running idle-command"));
    assert!(idle.contains("Running later-command"));
    assert!(!idle.contains("Unknown"));
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Completed,
        },
    );
    assert!(text(&draw(&mut chat, 64, 24)?).contains("Ran idle-command"));
    assert_eq!(chat.initial_prompt("next")?, Action::Submit("next".into()));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(2),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(2),
            name: "cocoa.wait".into(),
            truncated: false,
        },
    }));
    emit(
        &mut chat,
        EventKind::CapabilityOutput {
            operation: OperationId(2),
            output: ActivityOutput {
                text: "late stderr".into(),
                kind: ActivityOutputKind::Command,
                summary: None,
                truncated: false,
            },
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(2),
            status: ActivityStatus::Signalled {
                signal: rustix::process::Signal::TERM.as_raw(),
            },
        },
    );
    emit(
        &mut chat,
        EventKind::AssistantText {
            text: "stale answer".into(),
            truncated: false,
        },
    );
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(2),
        kind: EventKind::AssistantText {
            text: "Current preview.".into(),
            truncated: false,
        },
    }));
    click_activity(&mut chat, "Work Done")?;
    chat.input(key(KeyCode::Tab));
    chat.input(key(KeyCode::Down));
    chat.input(key(KeyCode::Right));
    let running = text(&draw(&mut chat, 64, 24)?);
    assert!(running.contains("Ran later-command · SIGTERM ▾"));
    assert!(running.contains("late stderr"));
    assert!(running.contains("Current preview."));
    assert!(!running.contains("stale answer"));
    assert!(!running.contains("cocoa.wait"));
    chat.complete("Second answer.");
    let complete = text(&draw(&mut chat, 64, 24)?);
    assert!(complete.contains("First answer."));
    assert!(complete.contains("Second answer."));
    assert!(complete.contains("Ran later-command · SIGTERM ▾"));
    Ok(())
}

#[test]
fn file_activities_change_tense_and_disclose_colored_applied_diffs() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    for (operation, title, active, past) in [
        (1, "Read note.rs", "Reading note.rs", "Read note.rs"),
        (2, "Write note.rs", "Writing note.rs", "Wrote note.rs"),
        (3, "Edit note.rs", "Editing note.rs", "Edited note.rs"),
    ] {
        emit(
            &mut chat,
            EventKind::CapabilityInput {
                operation: OperationId(operation),
                title: title.into(),
                text: "Never display these request details.".into(),
                summary: None,
                truncated: false,
            },
        );
        let working = text(&draw(&mut chat, 64, 32)?);
        assert!(working.contains(active), "{working}");
        if operation != 1 {
            emit(
                &mut chat,
                EventKind::CapabilityOutput {
                    operation: OperationId(operation),
                    output: ActivityOutput {
                        text:
                            "@@ -1,3 +1,3 @@\n pub fn retry_limit() -> u32 {\n-    3\n+    5\n }\n"
                                .into(),
                        kind: ActivityOutputKind::Diff,
                        summary: Some(ActivitySummary::LineChanges {
                            added: 1,
                            removed: 1,
                        }),
                        truncated: false,
                    },
                },
            );
        }
        emit(
            &mut chat,
            EventKind::CapabilityFinished {
                operation: OperationId(operation),
                status: ActivityStatus::Completed,
            },
        );
        let done = text(&draw(&mut chat, 64, 32)?);
        assert!(done.contains(past), "{done}");
        assert!(!done.contains(active), "{done}");
        assert!(!done.contains("Never display"), "{done}");
        if operation == 1 {
            continue;
        }
        chat.input(key(KeyCode::Tab));
        chat.input(key(KeyCode::Right));
        let expanded = draw(&mut chat, 64, 32)?;
        assert!(text(&expanded).contains(&format!("{past} +1 −1 ▾")));
        assert_eq!(cells(&expanded, "-    3")?[0].fg, Color::Rgb(242, 148, 137));
        assert_eq!(cells(&expanded, "+    5")?[0].fg, Color::Rgb(185, 203, 151));
        for content in [
            "@@ -1,3 +1,3 @@",
            "pub fn retry_limit()",
            "-    3",
            "+    5",
        ] {
            assert!(
                cells(&expanded, content)?
                    .iter()
                    .all(|cell| cell.bg == Color::Rgb(33, 24, 19))
            );
        }
        let plain = text(&expanded);
        let diff_row = plain
            .lines()
            .position(|line| line.contains("@@ -1,3 +1,3 @@"))
            .ok_or("missing diff header")?;
        let diff_line = plain.lines().nth(diff_row).ok_or("missing diff row")?;
        let diff_column = diff_line.find("@@").ok_or("missing diff column")?;
        assert_ne!(
            expanded[(diff_column as u16 - 1, diff_row as u16)].bg,
            Color::Rgb(33, 24, 19),
            "the diff background must not extend left of the diff text"
        );
        let first = plain
            .lines()
            .position(|line| line.contains("-    3"))
            .ok_or("missing deletion")? as u16;
        let last = plain
            .lines()
            .position(|line| line.contains("+    5"))
            .ok_or("missing addition")? as u16;
        let column = diff_column as u16;
        chat.input(mouse(
            MouseEventKind::Down(MouseButton::Left),
            column,
            first,
        ));
        assert_eq!(
            chat.input(mouse(
                MouseEventKind::Up(MouseButton::Left),
                column + 5,
                last
            )),
            Action::Copy("-    3\n+    5".into())
        );
        for width in [36, 18, 4, 1] {
            draw(&mut chat, width, 32)?;
        }
    }
    Ok(())
}

#[test]
fn streamed_layout_matches_fresh_layout_after_prefix_changes() -> Result<(), Box<dyn Error>> {
    for width in [12, 36, 80] {
        let mut streamed = started()?;
        let mut source = String::new();
        for delta in [
            "First line with words that wrap.\n\n",
            "Unicode café 漢字 👩‍💻 and e\u{301}.\n\n",
            "[reference][target]\n\n",
            "[target]: https://example.com\n\n",
            "| Name | Value |\n| --- | --- |\n| A | B |\n\n",
            "\n```rust\nlet value = 1;\n",
            "let next = 2;\n```\n\nLast line.",
            "\n\nAnother paragraph after the completed fence. λ",
        ] {
            source.push_str(delta);
            emit(
                &mut streamed,
                EventKind::AssistantText {
                    text: delta.into(),
                    truncated: false,
                },
            );
            let mut fresh = started()?;
            emit(
                &mut fresh,
                EventKind::AssistantText {
                    text: source.clone(),
                    truncated: false,
                },
            );
            let expected = draw(&mut fresh, width, 40)?;
            assert_eq!(draw(&mut streamed, width, 40)?, expected);
            draw(&mut streamed, width + 1, 40)?;
            assert_eq!(draw(&mut streamed, width, 40)?, expected);
        }
    }
    Ok(())
}

#[test]
fn cached_code_matches_fresh_rendering_after_streaming_and_theme_changes()
-> Result<(), Box<dyn Error>> {
    let mut streamed = started()?;
    let mut source = String::new();
    for delta in [
        "```rust\nlet café = 1;\n```\n\nFirst paragraph.",
        "\n\nA later paragraph reuses the completed block.",
        "\n\n```rust\nlet next = ",
        "2;\n```\n\nBoth blocks are complete.",
        "\n\n```unknown-language\nliteral café 漢字\n```\n\nDone.",
    ] {
        source.push_str(delta);
        emit(
            &mut streamed,
            EventKind::AssistantText {
                text: delta.into(),
                truncated: false,
            },
        );
        let mut fresh = started()?;
        emit(
            &mut fresh,
            EventKind::AssistantText {
                text: source.clone(),
                truncated: false,
            },
        );
        assert_eq!(draw(&mut streamed, 64, 40)?, draw(&mut fresh, 64, 40)?);
    }
    for theme in [
        Theme::MilkChocolate,
        Theme::TerminalNative,
        Theme::DarkChocolate,
    ] {
        streamed.set_theme(theme);
        let mut fresh = started()?;
        fresh.set_theme(theme);
        emit(
            &mut fresh,
            EventKind::AssistantText {
                text: source.clone(),
                truncated: false,
            },
        );
        assert_eq!(draw(&mut streamed, 64, 40)?, draw(&mut fresh, 64, 40)?);
    }
    Ok(())
}

#[test]
fn streaming_fences_and_unknown_or_invalid_code_never_hide_text() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    emit(
        &mut chat,
        EventKind::AssistantText {
            text: "**Working**\n\n```rust\nlet café = \"é".into(),
            truncated: false,
        },
    );
    let provisional = draw(&mut chat, 64, 24)?;
    assert!(
        cells(&provisional, "Working")?[0]
            .modifier
            .contains(Modifier::BOLD)
    );
    assert!(text(&provisional).contains("let café = \"é"));
    emit(
        &mut chat,
        EventKind::AssistantText {
            text: "\";\n```\n\nStill provisional.".into(),
            truncated: false,
        },
    );
    assert!(text(&draw(&mut chat, 64, 24)?).contains("Still provisional."));
    chat.complete("```not-a-language\n  raw *literal* &amp; λ\n```\n\n```diff\nnot a patch\n+not an edit\n```\n\n```rust\nlet final_value = 9;\n```\n\n**Finished**");
    for width in [64, 32, 64] {
        let view = draw(&mut chat, width, 32)?;
        assert!(!text(&view).contains("provisional"));
        assert!(text(&view).contains("raw *literal* &amp; λ"));
        assert!(text(&view).contains("not a patch"));
        assert!(text(&view).contains("+not an edit"));
        assert!(text(&view).contains("let final_value = 9;"));
        assert!(
            cells(&view, "Finished")?[0]
                .modifier
                .contains(Modifier::BOLD)
        );
    }
    Ok(())
}

#[test]
fn drag_copies_displayed_markdown_and_code_without_markup_or_soft_wraps()
-> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    chat.complete(
        "**alpha** beta `gamma` delta\nsecond line\n\n```rust\n  let café = 7;\n  café + 1\n```",
    );
    let view = draw(&mut chat, 22, 24)?;
    let plain = text(&view);
    let first = plain
        .lines()
        .position(|line| line.contains("alpha beta gamma"))
        .ok_or("missing prose")? as u16;
    let last = plain
        .lines()
        .position(|line| line.contains("second line"))
        .ok_or("missing second line")? as u16;
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), 2, first));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Up(MouseButton::Left), 12, last)),
        Action::Copy("alpha beta gamma delta\nsecond line".into())
    );
    draw(&mut chat, 22, 24)?;
    let first = plain
        .lines()
        .position(|line| line.contains("let café = 7;"))
        .ok_or("missing code")? as u16;
    let last = plain
        .lines()
        .position(|line| line.contains("café + 1"))
        .ok_or("missing final code")? as u16;
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), 2, first));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Up(MouseButton::Left), 11, last)),
        Action::Copy("  let café = 7;\n  café + 1".into())
    );
    let mut graphemes = started()?;
    graphemes.complete("**e**&#x301;, 👩&#x200d;💻");
    let view = draw(&mut graphemes, 32, 20)?;
    assert_eq!(view[(2, 4)].symbol(), "e\u{301}");
    assert_eq!(view[(3, 4)].symbol(), ",");
    assert_eq!(view[(5, 4)].symbol(), "👩‍💻");
    graphemes.input(mouse(MouseEventKind::Down(MouseButton::Left), 2, 4));
    assert_eq!(
        graphemes.input(mouse(MouseEventKind::Up(MouseButton::Left), 6, 4)),
        Action::Copy("e\u{301}, 👩‍💻".into())
    );
    Ok(())
}

#[test]
fn tables_reflow_and_literal_command_output_does_not_become_markdown() -> Result<(), Box<dyn Error>>
{
    let mut chat = started()?;
    emit(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Running printf".into(),
            text: "Command:\nprintf '**literal input**\\n```diff\\n+not an edit\\n```'".into(),
            summary: None,
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityOutput {
            operation: OperationId(1),
            output: ActivityOutput {
                text: "**literal output**\n```diff\n+not an edit\n```".into(),
                kind: ActivityOutputKind::Command,
                summary: None,
                truncated: false,
            },
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Completed,
        },
    );
    chat.complete("# Table\n\nThis is **formatted** prose with [docs](https://example.com/a/long/path/that/wraps).\n\n| Name | Value |\n| :--- | ---: |\n| café | 23 |\n\nSafe &#27;[31m text.");
    let wide = draw(&mut chat, 64, 24)?;
    assert!(text(&wide).contains('┌'));
    assert!(text(&wide).contains("café"));
    assert!(!text(&wide).contains("&#27;"));
    assert!(!text(&wide).contains('\x1b'));
    let wrapped = draw(&mut chat, 32, 32)?;
    assert!(text(&wrapped).contains('┌'));
    assert!(
        cells(&wrapped, "Table")?[0]
            .modifier
            .contains(Modifier::BOLD)
    );
    let narrow = draw(&mut chat, 12, 32)?;
    assert!(
        !text(&narrow).contains('┌'),
        "narrow tables must not wrap box borders"
    );
    assert!(text(&narrow).contains("café"));
    assert!(text(&draw(&mut chat, 64, 24)?).contains('┌'));
    click_activity(&mut chat, "Work Done")?;
    chat.input(key(KeyCode::Tab));
    chat.input(key(KeyCode::Right));
    let expanded = draw(&mut chat, 64, 32)?;
    assert!(text(&expanded).contains("**literal output**"));
    assert!(text(&expanded).contains("```diff"));
    assert!(text(&expanded).contains("+not an edit"));
    Ok(())
}

#[test]
fn references_and_nested_code_keep_their_document_context() -> Result<(), Box<dyn Error>> {
    let mut chat = started()?;
    chat.complete("Read [the docs][guide].\n\n```rust\nlet value = 3;\n```\n\n> **Quoted**\n>\n> ```rust\n> let nested = 4;\n> ```\n\n[guide]: https://example.com/docs\n");
    let view = draw(&mut chat, 80, 32)?;
    let value = text(&view);
    assert!(value.contains("the docs"));
    assert!(value.contains("https://example.com/docs"));
    assert!(!value.contains("[guide]"));
    assert!(value.contains("let value = 3;"));
    assert!(value.contains("let nested = 4;"));
    assert!(cells(&view, "Quoted")?[0].modifier.contains(Modifier::BOLD));
    let mut nested = started()?;
    nested.complete("> | A | B |\n> | --- | --- |\n> | x | y |");
    for width in [16, 64] {
        let view = text(&draw(&mut nested, width, 32)?);
        assert!(!view.contains('┌'));
        assert!(view.contains("> | x | y |"));
    }
    Ok(())
}

#[test]
fn highlighting_limits_preserve_all_code_lines_and_long_unicode() -> Result<(), Box<dyn Error>> {
    for count in [511, 512, 513] {
        let mut chat = started()?;
        let source = (0..count)
            .map(|index| format!("// row-{index:04}\n"))
            .collect::<String>();
        chat.complete(&format!("```rust\n{source}```"));
        let value = text(&draw(&mut chat, 40, 530)?);
        assert_eq!(value.matches("row-").count(), count);
        assert!(value.contains(&format!("row-{:04}", count - 1)));
    }
    let mut chat = started()?;
    chat.complete(&format!(
        "```rust\nlet payload = \"{}\";\nlast_line();\n```",
        "界".repeat(1500)
    ));
    let value = text(&draw(&mut chat, 80, 90)?);
    assert_eq!(value.matches('界').count(), 1500);
    assert!(value.contains("last_line();"));
    Ok(())
}

fn started() -> Result<Chat, Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    chat.input(key(KeyCode::Char('q')));
    assert_eq!(chat.input(key(KeyCode::Enter)), Action::Submit("q".into()));
    Ok(chat)
}

fn emit(chat: &mut Chat, kind: EventKind) {
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind,
    }));
}

fn click_activity(chat: &mut Chat, label: &str) -> Result<(), Box<dyn Error>> {
    let view = text(&draw(chat, 64, 32)?);
    let (row, column) = view
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find(label).map(|column| (row, column)))
        .ok_or_else(|| format!("missing activity {label}: {view}"))?;
    chat.input(mouse(
        MouseEventKind::Down(MouseButton::Left),
        column as u16,
        row as u16,
    ));
    chat.input(mouse(
        MouseEventKind::Up(MouseButton::Left),
        column as u16,
        row as u16,
    ));
    Ok(())
}

fn draw(chat: &mut Chat, width: u16, height: u16) -> Result<Buffer, Box<dyn Error>> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| chat.render(frame))?;
    Ok(terminal.backend().buffer().clone())
}

fn text(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(usize::from(buffer.area.width))
        .map(|row| row.iter().map(Cell::symbol).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn cells<'a>(buffer: &'a Buffer, needle: &str) -> Result<&'a [Cell], Box<dyn Error>> {
    for row in buffer.content.chunks(usize::from(buffer.area.width)) {
        if let Some(cells) = row
            .windows(needle.chars().count())
            .find(|cells| cells.iter().map(Cell::symbol).collect::<String>() == needle)
        {
            return Ok(cells);
        }
    }
    Err(format!("missing {needle:?} in:\n{}", text(buffer)).into())
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

fn linked_runs(output: &[u8]) -> Result<Vec<(String, String)>, Box<dyn Error>> {
    let mut remaining = std::str::from_utf8(output)?;
    let mut links = Vec::new();
    while let Some((_, after)) = remaining.split_once("\x1b]8;") {
        let (parameters, after) = after.split_once(';').ok_or("missing OSC parameters")?;
        assert!(parameters.starts_with("id="));
        let (destination, after) = after.split_once("\x1b\\").ok_or("unterminated OSC")?;
        assert!(!destination.chars().any(char::is_control));
        let (fragment, after) = after
            .split_once("\x1b]8;;\x1b\\")
            .ok_or("hyperlink not closed in the same output run")?;
        assert!(!fragment.chars().any(char::is_control));
        links.push((destination.to_owned(), fragment.to_owned()));
        remaining = after;
    }
    Ok(links)
}
