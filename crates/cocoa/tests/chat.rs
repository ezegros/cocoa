//! Terminal view behavior through input events, observations and rendered buffers.

use std::{error::Error, time::Duration};

use cocoa::{
    agents::{AgentUpdate, AgentUpdateKind},
    chat::{Action, Chat, PasteError},
};
use cocoa_core::{
    engine::{ActivityStatus, Event as EngineEvent, EventKind, Observation},
    model::{Image, UserInput},
    store::{InvocationId, OperationId},
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
};

#[test]
fn rewind_is_local_and_blocked_during_a_turn() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(4096)?;
    type_text(&mut chat, "/rewind");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/rewind".into())
    );
    assert!(!chat.has_draft());
    type_text(&mut chat, "first prompt");
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(_)
    ));
    type_text(&mut chat, "/rewind");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert!(chat.has_draft());
    assert!(render(&mut chat, 100, 30)?.contains("Cannot switch sessions while a turn is active"));
    Ok(())
}

#[test]
fn rewind_restores_prompt_with_original_image_markers() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(4096)?;
    let image = Image::from_png(b"\x89PNG\r\n\x1a\nrestored")?;
    let input = UserInput::WithImages {
        text: "Review [Image 7] and [Image 12]".into(),
        images: vec![image.clone(), image],
    };
    type_text(&mut chat, "discard this draft");
    chat.restore_prompt(&input)?;
    assert!(chat.has_draft());
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(input)
    );
    Ok(())
}

#[test]
fn rewind_prompt_rejection_preserves_the_draft() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "keep me");
    assert!(matches!(
        chat.restore_prompt(&UserInput::Text("x".repeat(65))),
        Err(PasteError::InputLimit)
    ));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(UserInput::Text("keep me".into()))
    );
    Ok(())
}

#[test]
fn theme_command_selects_live_and_stays_out_of_history() -> Result<(), Box<dyn Error>> {
    use cocoa::theme::Theme;

    let mut chat = Chat::new(1024)?;
    type_text(&mut chat, "/theme");
    assert!(render(&mut chat, 80, 24)?.contains("/theme milk-chocolate"));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(chat.theme(), Theme::DarkChocolate);
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(chat.theme(), Theme::MilkChocolate);
    let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
    terminal.draw(|frame| chat.render(frame))?;
    assert_eq!(
        terminal.backend().buffer()[(0, 0)].bg,
        ratatui::style::Color::Rgb(245, 237, 223)
    );
    assert!(chat.can_submit());
    assert!(!render(&mut chat, 80, 24)?.contains("/theme"));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );

    for (name, theme) in [
        ("terminal-native", Theme::TerminalNative),
        ("cacao-grove", Theme::CacaoGrove),
        ("dark-chocolate", Theme::DarkChocolate),
    ] {
        type_text(&mut chat, &format!("/theme {name}"));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Redraw
        );
        assert_eq!(chat.theme(), theme);
        terminal.draw(|frame| chat.render(frame))?;
        assert_eq!(
            terminal.backend().buffer()[(0, 0)].bg,
            theme.palette().background
        );
        assert!(chat.can_submit());
    }
    type_text(&mut chat, "/theme invalid");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(chat.theme(), Theme::DarkChocolate);
    assert!(render(&mut chat, 80, 24)?.contains("Use /theme to select"));

    type_text(&mut chat, "/theme cacao");
    assert_eq!(
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(chat.theme(), Theme::CacaoGrove);
    Ok(())
}

#[test]
fn welcome_keeps_the_largest_mark_that_fits_the_draft() -> Result<(), Box<dyn Error>> {
    for (height, draft_lines, large) in [
        (29, 3, true),
        (29, 4, true),
        (28, 3, true),
        (28, 4, false),
        (27, 4, false),
        (16, 3, false),
    ] {
        let mut chat = Chat::new(4096)?;
        type_text(&mut chat, &vec!["draft"; draft_lines].join("\n"));
        let view = render(&mut chat, 80, height)?;
        let mark_columns = (0..80)
            .filter(|column| {
                view.lines().any(|line| {
                    line.chars()
                        .nth(*column)
                        .is_some_and(|symbol| matches!(symbol, '█' | '▀' | '▄'))
                })
            })
            .count();
        assert!(mark_columns > 0, "{view}");
        assert_eq!(mark_columns > 16, large, "{view}");
        assert!(view.contains("cocoa"), "{view}");
        assert!(view.contains("Got an idea?"), "{view}");
        assert!(view.contains("sweet."), "{view}");
    }
    Ok(())
}

#[test]
fn welcome_resizes_and_never_enters_the_conversation() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(4096)?;
    for (width, height, symbol) in [
        (80, 30, true),
        (68, 30, true),
        (80, 24, true),
        (40, 18, true),
        (32, 12, false),
        (80, 30, true),
    ] {
        let view = render(&mut chat, width, height)?;
        assert!(view.contains("cocoa"), "{view}");
        assert_eq!(view.contains(['█', '▀', '▄']), symbol, "{view}");
        if symbol {
            let invitation = view
                .lines()
                .skip_while(|line| !line.contains("Got an idea?"))
                .take(2)
                .map(|line| {
                    line.trim_matches(|character: char| {
                        character.is_whitespace() || matches!(character, '█' | '▀' | '▄')
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(
                invitation.trim(),
                "Got an idea? Let's make something sweet.",
                "{view}"
            );
        } else {
            assert!(!view.contains("Got an idea?"), "{view}");
        }
        assert_eq!(view.lines().count(), usize::from(height));
    }
    type_text(&mut chat, "hello");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("hello".into())
    );
    chat.complete("Hello back.");
    let view = render(&mut chat, 80, 24)?;
    assert!(!view.contains("cocoa"), "{view}");
    assert!(!view.contains(['█', '▀', '▄']), "{view}");
    type_text(&mut chat, "/copy");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Copy("Hello back.".into())
    );
    Ok(())
}

#[test]
fn working_starts_expanded_and_collapses_on_completion() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(4096)?;
    type_text(&mut chat, "read a file");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "cocoa.read".into(),
            truncated: false,
        },
    }));
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    let expanded = render(&mut chat, 64, 24)?;
    assert!(expanded.contains("Toasting ▾"), "{expanded}");
    assert!(expanded.contains("Reading file"), "{expanded}");
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    let collapsed = render(&mut chat, 64, 24)?;
    assert!(collapsed.contains("Toasting ▸"), "{collapsed}");
    assert!(!collapsed.contains("Reading file"), "{collapsed}");
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    chat.complete("Done");
    let completed = render(&mut chat, 64, 24)?;
    assert!(completed.contains("Work Done ▸"), "{completed}");
    assert!(!completed.contains("Reading file"), "{completed}");
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let reopened = render(&mut chat, 64, 24)?;
    assert!(reopened.contains("Work Done ▾"), "{reopened}");
    assert!(reopened.contains("Read file"), "{reopened}");
    Ok(())
}

#[test]
fn alias_shortcuts_cycle_in_both_directions() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(4096)?;
    assert_eq!(
        chat.input(key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
        Action::CycleAlias
    );
    for character in ['t', 'T'] {
        assert_eq!(
            chat.input(key(
                KeyCode::Char(character),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Action::CycleAliasPrevious
        );
    }
    Ok(())
}

#[test]
fn skill_commands_complete_and_submit_exact_ids_with_arguments() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let skill_directory = directory.path().join(".cocoa/skills/review");
    std::fs::create_dir_all(&skill_directory)?;
    let skill_path = skill_directory.join("SKILL.md");
    std::fs::write(
        &skill_path,
        "---\nname: review\ndescription: Review changes\n---\nCheck the diff.\n",
    )?;
    let skills = cocoa_tools::skill_commands(directory.path(), None)?;
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "review");
    assert_eq!(skills[0].description, "Review changes");
    assert_eq!(
        skills[0].id,
        std::fs::canonicalize(&skill_path)?.to_string_lossy()
    );
    let id = serde_json::to_string(&skills[0].id)?;
    let mut chat = Chat::new(4096)?;
    chat.set_skills(skills);
    type_text(&mut chat, "$");
    let view = render(&mut chat, 100, 24)?;
    assert!(view.contains("$review"));
    assert!(!view.contains("Connect a provider"));
    type_text(&mut chat, "rev");
    let view = render(&mut chat, 100, 24)?;
    assert!(view.contains("$review"));
    assert!(view.contains("Review changes"));
    assert_eq!(
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE)),
        Action::Redraw
    );
    type_text(&mut chat, " inspect src/main.rs");
    let action = chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    let Action::Submit(input) = action else {
        return Err(format!("expected skill submission, got {action:?}").into());
    };
    assert!(
        input
            .text()
            .contains(&format!("cocoa.load_skill({{id:{id}}})"))
    );
    assert!(input.text().ends_with("\n\n$review inspect src/main.rs"));
    chat.complete("Reviewed");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert!(render(&mut chat, 100, 24)?.contains("$review inspect src/main.rs"));
    Ok(())
}

#[test]
fn skill_commands_preserve_local_commands_and_reject_ambiguous_names() -> Result<(), Box<dyn Error>>
{
    let skill = cocoa_tools::SkillCommand {
        id: "/skills/review/SKILL.md".into(),
        name: "review".into(),
        description: "Review changes".into(),
    };
    let mut chat = Chat::new(4096)?;
    let mut duplicate = skill.clone();
    duplicate.id = "/other/review/SKILL.md".into();
    let mut builtin = skill.clone();
    builtin.name = "mcp".into();
    chat.set_skills(vec![skill.clone(), duplicate, builtin]);
    type_text(&mut chat, "/mcp");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/mcp".into())
    );
    type_text(&mut chat, "/");
    let view = render(&mut chat, 100, 24)?;
    assert!(view.contains("/mcp"));
    assert!(!view.contains("Review changes"));
    assert!(!view.contains("/review"));
    chat.input(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut chat, "/review");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/review".into())
    );
    type_text(&mut chat, "$mcp");
    let Action::Submit(input) = chat.input(key(KeyCode::Enter, KeyModifiers::NONE)) else {
        return Err("skill names may match built-in commands".into());
    };
    assert!(input.text().contains("cocoa.load_skill"));
    chat.complete("Done");
    type_text(&mut chat, "Please use $review for this");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert!(render(&mut chat, 100, 24)?.contains("Multiple skills have this name"));
    chat.set_skills(vec![skill]);
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(_)
    ));
    chat.set_message_steering(false);
    type_text(&mut chat, "$review");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    chat.complete("Done");
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(_)
    ));
    Ok(())
}

#[test]
fn skill_request_expansion_respects_input_limit() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    chat.set_skills(vec![cocoa_tools::SkillCommand {
        id: "/skills/review/SKILL.md".into(),
        name: "review".into(),
        description: "Review changes".into(),
    }]);
    type_text(&mut chat, "$review");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert!(render(&mut chat, 100, 24)?.contains("Skill request exceeds"));
    for (budget, expected) in [(18, "123456 $rev tail"), (19, "123456 $review tail")] {
        let mut chat = Chat::new(budget)?;
        chat.set_skills(vec![cocoa_tools::SkillCommand {
            id: "/skills/review/SKILL.md".into(),
            name: "review".into(),
            description: "Review changes".into(),
        }]);
        type_text(&mut chat, "123456 $rev tail");
        for _ in " tail".chars() {
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        }
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(render(&mut chat, 100, 24)?.contains(expected));
    }
    Ok(())
}

#[test]
fn skill_completion_preserves_surrounding_multiline_text_and_loads_each_reference_once()
-> Result<(), Box<dyn Error>> {
    let skills = vec![
        cocoa_tools::SkillCommand {
            id: "/skills/review/SKILL.md".into(),
            name: "review".into(),
            description: "Review changes".into(),
        },
        cocoa_tools::SkillCommand {
            id: "/skills/check/SKILL.md".into(),
            name: "check".into(),
            description: "Check behavior".into(),
        },
    ];
    for separator in [" ", "\n", "\u{2003}"] {
        let mut chat = Chat::new(4096)?;
        chat.set_skills(skills.clone());
        chat.paste_text(&format!(
            "Inspect π🦀{separator}$rev later with $check and $review"
        ))?;
        for _ in "v later with $check and $review".chars() {
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        }
        let view = render(&mut chat, 100, 24)?;
        assert!(view.contains("Review changes"), "{view}");
        assert!(!view.contains("Check behavior"));
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
        let Action::Submit(input) = chat.input(key(KeyCode::Enter, KeyModifiers::NONE)) else {
            return Err("expected inline skill submission".into());
        };
        assert!(input.text().ends_with(&format!(
            "Inspect π🦀{separator}$review later with $check and $review"
        )));
        assert_eq!(input.text().matches("cocoa.load_skill").count(), 2);
        assert!(input.text().contains("/skills/review/SKILL.md"));
        assert!(input.text().contains("/skills/check/SKILL.md"));
    }
    let mut chat = Chat::new(4096)?;
    chat.set_skills(skills);
    type_text(&mut chat, "cost$rev $UNKNOWN $5");
    assert!(!render(&mut chat, 100, 24)?.contains("Review changes"));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(UserInput::Text("cost$rev $UNKNOWN $5".into()))
    );
    Ok(())
}

#[test]
fn image_paths_replace_only_the_path_and_preserve_draft_limits() -> Result<(), Box<dyn Error>> {
    let image = clipboard_image([30, 70, 120, 255])?;
    for (source, expected) in [
        (r"/tmp/Screen\ λ\ \(1\).PNG", "/tmp/Screen λ (1).PNG"),
        (r#""/tmp/Screen λ (1).png""#, "/tmp/Screen λ (1).png"),
        (r"'/tmp/Screen'\''s.png'", "/tmp/Screen's.png"),
        ("file:///tmp/Screen%20%CE%BB.png", "/tmp/Screen λ.png"),
        ("./screen.jpeg", "./screen.jpeg"),
        ("../screen.webp", "../screen.webp"),
    ] {
        let mut chat = Chat::new(4096)?;
        chat.paste_text(&format!("What's in {source} "))?;
        let path = chat.image_path().ok_or("missing image path")?;
        assert_eq!(path.path, std::path::Path::new(expected), "{source}");
        chat.paste_image_path(&path, image.clone())?;
        assert!(render(&mut chat, 64, 14)?.contains("What's in [Image 1]"));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(UserInput::WithImages {
                text: "What's in [Image 1] ".into(),
                images: vec![image.clone()],
            })
        );
    }
    for text in [
        "@/tmp/screen.png",
        "https://example.com/screen.png",
        "file://remote-host/tmp/screen.png",
        "./notes.txt",
        "'/tmp/incomplete.png",
    ] {
        let mut chat = Chat::new(4096)?;
        chat.paste_text(text)?;
        assert!(chat.image_path().is_none(), "{text}");
    }

    let mut chat = Chat::new(4096)?;
    let source = format!(
        "What is shown in this screenshot? file:///tmp/{}.png",
        "long-file-name".repeat(12)
    );
    for character in source.chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
        render(&mut chat, 36, 12)?;
    }
    let path = chat.image_path().ok_or("missing wrapped path")?;
    assert_eq!(
        path.path,
        std::path::PathBuf::from(format!("/tmp/{}.png", "long-file-name".repeat(12)))
    );
    chat.paste_image_path(&path, image.clone())?;
    let view = render(&mut chat, 36, 12)?;
    assert!(view.contains("What is shown in this"), "{view}");
    assert!(view.contains("screenshot? [Image 1]"), "{view}");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(UserInput::WithImages {
            text: "What is shown in this screenshot? [Image 1]".into(),
            images: vec![image.clone()],
        })
    );

    let mut chat = Chat::new(4096)?;
    let prefix = format!("{} ", "x".repeat(50));
    chat.paste_text(&format!("{prefix}/tmp/{}.png", "long-name".repeat(20)))?;
    render(&mut chat, 36, 12)?;
    let path = chat.image_path().ok_or("missing right-edge path")?;
    chat.paste_image_path(&path, image.clone())?;
    render(&mut chat, 36, 12)?;
    chat.paste_text("!")?;
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(UserInput::WithImages {
            text: format!("{prefix}[Image 1]!"),
            images: vec![image.clone()],
        })
    );

    let mut chat = Chat::new(image.data_url().len() + "[Image 1] ".len())?;
    chat.paste_text("/tmp/screen.png ")?;
    let path = chat.image_path().ok_or("missing budget path")?;
    chat.paste_image_path(&path, image.clone())?;
    assert!(matches!(chat.paste_text("x"), Err(PasteError::InputLimit)));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(UserInput::WithImages {
            text: "[Image 1] ".into(),
            images: vec![image],
        })
    );
    Ok(())
}

#[test]
fn clipboard_images_follow_markers_through_submission_busy_drafts_and_recall()
-> Result<(), Box<dyn Error>> {
    let first = clipboard_image([220, 20, 40, 255])?;
    let second = clipboard_image([20, 80, 230, 255])?;
    let mut chat = Chat::new(4096)?;
    chat.set_message_steering(false);
    for modifiers in [KeyModifiers::CONTROL, KeyModifiers::SUPER] {
        assert_eq!(
            chat.input(key(KeyCode::Char('v'), modifiers)),
            Action::Paste
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert_eq!(
                chat.input(Event::Key(KeyEvent {
                    kind,
                    ..KeyEvent::new(KeyCode::Char('v'), modifiers)
                })),
                Action::None
            );
        }
    }
    type_text(&mut chat, "Compare: ");
    chat.paste_image(first.clone())?;
    chat.input(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    chat.paste_image(second.clone())?;
    let view = render(&mut chat, 64, 14)?;
    assert!(view.contains("[Image 2]Compare: [Image 1]"), "{view}");
    assert!(!view.contains("base64"));
    let expected = UserInput::WithImages {
        text: "[Image 2]Compare: [Image 1]".into(),
        images: vec![second, first.clone()],
    };
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(expected)
    );

    type_text(&mut chat, "next λ ");
    chat.paste_image(first.clone())?;
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    chat.complete("comparison");
    render(&mut chat, 64, 14)?;
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert!(render(&mut chat, 64, 14)?.contains("[Image 2]Compare: [Image 1]"));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(UserInput::WithImages {
            text: "next λ [Image 3]".into(),
            images: vec![first],
        })
    );

    chat.complete("next answer");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("next λ ".into())
    );
    chat.complete("text only");
    assert!(!render(&mut chat, 64, 14)?.contains("data:image/png"));
    chat.set_message_steering(true);
    chat.initial_prompt("start a new task")?;
    let image = clipboard_image([17, 42, 93, 255])?;
    chat.paste_text("Use this ")?;
    chat.paste_image(image.clone())?;
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Steer(UserInput::WithImages {
            text: "Use this [Image 4]".into(),
            images: vec![image],
        })
    );
    Ok(())
}

#[test]
fn clipboard_paste_limits_are_atomic_and_clearing_discards_images() -> Result<(), Box<dyn Error>> {
    let image = clipboard_image([1, 2, 3, 255])?;
    let mut chat = Chat::new(image.data_url().len() + "[Image 1]".len() + "é".len())?;
    type_text(&mut chat, "é");
    chat.paste_image(image.clone())?;
    assert!(matches!(chat.paste_text("x"), Err(PasteError::InputLimit)));
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    chat.input(key(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("é".into())
    );

    for clear in [
        key(KeyCode::Char('u'), KeyModifiers::CONTROL),
        key(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
        let mut chat = Chat::new(4096)?;
        for _ in 0..4 {
            chat.paste_image(image.clone())?;
        }
        assert!(matches!(
            chat.paste_image(image.clone()),
            Err(PasteError::ImageLimit)
        ));
        chat.input(clear);
        type_text(&mut chat, "clean");
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("clean".into())
        );
    }

    let mut chat = Chat::new(64)?;
    chat.paste_text("é\r\nx\ry\t")?;
    assert!(matches!(
        chat.paste_text("\x1b[31m"),
        Err(PasteError::ControlText)
    ));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("é\nx\ny    ".into())
    );
    Ok(())
}

#[test]
fn transcript_refresh_preserves_drafts_images_cursors_and_recall() -> Result<(), Box<dyn Error>> {
    use cocoa_core::store::{Record, RecordId, RecordKind, RecordSequence, StoredRecord};

    let records = [StoredRecord {
        id: RecordId::try_from(1)?,
        sequence: RecordSequence::try_from(1)?,
        record: Record {
            invocation: InvocationId(1),
            kind: RecordKind::InputAccepted {
                input: "canonical history".into(),
            },
        },
    }];
    let image = clipboard_image([1, 2, 3, 255])?;
    for recalled in [false, true] {
        let mut chat = Chat::new(4096)?;
        chat.paste_text("previous ")?;
        chat.paste_image(image.clone())?;
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        chat.paste_text("next λ draft ")?;
        chat.paste_image(image.clone())?;
        chat.input(key(KeyCode::Home, KeyModifiers::NONE));
        render(&mut chat, 64, 16)?;
        if recalled {
            chat.input(key(KeyCode::Up, KeyModifiers::NONE));
            chat.input(key(KeyCode::Up, KeyModifiers::NONE));
        }
        chat.provider_paused("retry later");
        chat.refresh_transcript(&records);
        assert!(chat.can_submit());
        let view = render(&mut chat, 64, 16)?;
        assert!(view.contains("canonical history"), "{view}");
        assert!(!view.contains("retry later"), "{view}");
        if recalled {
            assert!(view.contains("│  previous [Image 1]"), "{view}");
            chat.input(key(KeyCode::Down, KeyModifiers::NONE));
        }
        chat.paste_text("kept ")?;
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(UserInput::WithImages {
                text: "kept next λ draft [Image 2]".into(),
                images: vec![image.clone()],
            })
        );
        chat.refresh_transcript(&records);
        chat.input(key(KeyCode::Up, KeyModifiers::NONE));
        assert!(render(&mut chat, 64, 16)?.contains("kept next λ draft [Image 2]"));
        chat.restore_transcript(&records);
        chat.input(key(KeyCode::Down, KeyModifiers::NONE));
        chat.input(key(KeyCode::Up, KeyModifiers::NONE));
        assert!(!chat.has_draft());
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
    }
    Ok(())
}

#[test]
fn completions_fit_the_combined_text_and_image_budget() -> Result<(), Box<dyn Error>> {
    for accept in [KeyCode::Tab, KeyCode::Enter] {
        for budget in [2, 5, 6] {
            let mut chat = Chat::new(budget)?;
            type_text(&mut chat, "/l");
            chat.input(key(accept, KeyModifiers::NONE));
            chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Command(if budget < 6 { "/l" } else { "/login" }.into())
            );
        }
    }
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("long-name.txt"), "body")?;
    let image = clipboard_image([10, 20, 30, 255])?;
    let completed = "[Image 1] @long-name.txt ";
    for accept in [KeyCode::Tab, KeyCode::Enter] {
        for fits in [false, true] {
            let budget = image.data_url().len() + completed.len() - usize::from(!fits);
            let mut chat = Chat::new(budget)?;
            chat.set_working_directory(directory.path());
            chat.paste_image(image.clone())?;
            chat.paste_text(" @lo")?;
            settle_files(&mut chat)?;
            chat.input(key(accept, KeyModifiers::NONE));
            chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit(UserInput::WithImages {
                    text: if fits { completed } else { "[Image 1] @lo" }.into(),
                    images: vec![image.clone()],
                })
            );
        }
    }
    Ok(())
}

#[test]
fn copy_command_copies_latest_completed_response_without_local_notices()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(4096)?;
    type_text(&mut chat, "first question");
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(_)
    ));
    chat.complete("First answer");
    type_text(&mut chat, "second question");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    let answer = "## Second answer\n\n- **Bold** and 🦀\n\n";
    chat.complete(answer);
    chat.notice("Local command output");
    type_text(&mut chat, "/cop");
    assert!(render(&mut chat, 80, 24)?.contains("Copy the last agent response"));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    for _ in 0..2 {
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Copy(answer.into())
        );
        assert!(chat.can_submit());
        type_text(&mut chat, "/copy");
    }
    Ok(())
}

#[test]
fn copy_command_without_a_response_shows_a_warning() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    chat.notice("Not an agent response");
    type_text(&mut chat, "/copy");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert!(render(&mut chat, 80, 24)?.contains("No agent response to copy yet."));
    assert!(chat.can_submit());
    Ok(())
}

#[test]
fn slash_completion_requires_another_enter_and_commands_stay_out_of_prompt_history()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    type_text(&mut chat, "/");
    let view = render(&mut chat, 80, 24)?;
    assert!(view.contains("/login     Connect a provider"));
    assert!(view.contains("/sessions  Search saved conversations"));
    assert!(view.contains("Disconnect a provider"));
    assert!(view.contains("Select an alias or provider model"));
    assert!(view.contains("Edit alias model choices and priorities"));
    assert!(!view.split_whitespace().any(|word| word == "/alias"));
    assert!(!view.contains("/preset"));
    assert!(!view.contains("Select a model for this session"));
    assert!(view.contains("Manage ChatGPT usage"));
    for _ in 0..2 {
        chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    }
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    ));
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command(command) if command == "/aliases"
    ));
    type_text(&mut chat, "/login openr");
    assert!(render(&mut chat, 80, 24)?.contains("/login openrouter"));
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    ));
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command(command) if command == "/login openrouter"
    ));
    assert!(!render(&mut chat, 80, 24)?.contains("/login"));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    ));
    type_text(&mut chat, "/log");
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command(command) if command == "/logout"
    ));
    type_text(&mut chat, "/model");
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command(command) if command == "/model"
    ));
    type_text(&mut chat, "/usage");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/usage".into())
    );
    assert!(
        matches!(chat.initial_prompt("initial question")?, Action::Submit(input) if input.text() == "initial question")
    );
    type_text(&mut chat, "/aliases");
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command(command) if command == "/aliases"
    ));
    assert!(!render(&mut chat, 80, 24)?.contains("/aliases"));
    chat.complete("answer");
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    ));
    Ok(())
}

#[test]
fn model_completion_and_selection_stay_local_and_out_of_history() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    type_text(&mut chat, "/mo");
    let view = render(&mut chat, 80, 24)?;
    assert!(view.contains("/models"));
    assert!(view.contains("Select an alias or provider model"));
    assert!(!view.contains("Select a model for this session"));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/models".into())
    );
    type_text(&mut chat, "/model");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model".into())
    );
    type_text(&mut chat, "/model openai/org/model");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model openai/org/model".into())
    );
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn control_p_opens_the_model_picker_without_editing_the_draft() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    type_text(&mut chat, "draft");
    assert_eq!(
        chat.input(key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        Action::Command("/models".into())
    );
    chat.input(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    )));
    type_text(&mut chat, "!");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("draft!".into())
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        Action::None
    );
    chat.complete("answer");
    type_text(&mut chat, "/m");
    assert_eq!(
        chat.input(key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        Action::Redraw
    );
    assert!(render(&mut chat, 80, 24)?.contains("/m"));
    Ok(())
}

#[test]
fn alias_completion_preserves_order_and_executes_exact_names_without_prefix_expansion()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    chat.set_aliases(&["low".into(), "high".into(), "high-review".into()]);
    type_text(&mut chat, "/model ");
    let view = render(&mut chat, 80, 24)?;
    assert!(
        matches!((view.find("/model low"), view.find("/model high")), (Some(low), Some(high)) if low < high),
        "{view}"
    );
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model high".into())
    );
    for command in [
        "/model high",
        "/model missing",
        "/model High",
        "/model high extra",
    ] {
        type_text(&mut chat, command);
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Command(command.into())
        );
    }
    type_text(&mut chat, "/model h");
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model high-review".into())
    );
    type_text(&mut chat, "/model h");
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model h".into())
    );
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn model_commands_complete_alias_names() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    chat.set_aliases(&["deep".into(), "fast".into()]);
    for (prefix, expected, absent) in [
        ("/models d", "/models deep", "/models fast"),
        ("/model d", "/model deep", "/model fast"),
    ] {
        type_text(&mut chat, prefix);
        let view = render(&mut chat, 80, 24)?;
        assert!(view.contains(expected), "{view}");
        assert!(!view.contains(absent), "{view}");
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Redraw
        );
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Command(expected.into())
        );
    }
    for command in ["/models fast", "/model fast", "/models missing"] {
        type_text(&mut chat, command);
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Command(command.into())
        );
    }
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    type_text(&mut chat, "/alias d");
    let view = render(&mut chat, 80, 24)?;
    assert!(!view.contains("/alias deep"), "{view}");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/alias d".into())
    );
    Ok(())
}

#[test]
fn initial_prompt_accepts_eight_mib_of_utf8_and_rejects_one_more_byte() -> Result<(), Box<dyn Error>>
{
    let limit = 8 * 1024 * 1024;
    let prompt = "é".repeat(limit / 2);
    let mut chat = Chat::new(limit)?;
    assert!(chat.initial_prompt(&format!("{prompt}x")).is_err());
    assert!(
        matches!(chat.initial_prompt(&prompt)?, Action::Submit(input) if input.text() == prompt)
    );
    assert!(Chat::new(limit + 1).is_err());
    Ok(())
}

#[test]
fn decorative_labels_ignore_model_and_tool_events_and_reset_for_each_turn()
-> Result<(), Box<dyn Error>> {
    let labels = [
        "Toasting",
        "Blending",
        "Refining",
        "Winnowing",
        "Conching",
        "Setting",
        "Finishing",
        "Tempering",
        "Stirring",
        "Cracking",
        "Melting",
        "Milling",
    ];
    let mut cases = Vec::with_capacity(labels.len() * 2 + 1);
    for (index, label) in labels.into_iter().enumerate() {
        let start = Duration::from_millis(index as u64 * 5760);
        cases.push((start, label, " (o|o) ", "(o)"));
        cases.push((
            start + Duration::from_nanos(5_759_999_999),
            label,
            " (|||) ",
            "(|)",
        ));
    }
    cases.push((Duration::from_millis(69_120), "Toasting", " (o|o) ", "(o)"));
    for (elapsed, label, full, mini) in cases {
        let mut chat = Chat::new(128)?;
        type_text(&mut chat, "question");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        type_text(&mut chat, "next λ");
        chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        chat.tick(elapsed);
        let initial = render(&mut chat, 64, 20)?;
        assert!(initial.contains(&format!("╭ {full}{label} ─")), "{initial}");
        assert!(initial.contains(&format!("  {mini} {label}")), "{initial}");
        assert_eq!(initial.matches(full).count(), 1, "{initial}");
        assert_eq!(initial.matches(mini).count(), 1, "{initial}");
        for kind in [
            EventKind::ModelStarted { round: 0 },
            EventKind::ReasoningStarted { item: 2 },
            EventKind::ReasoningText {
                item: 2,
                text: "Compare the two file versions.".into(),
                truncated: true,
            },
            EventKind::ReasoningFinished { item: 2 },
            EventKind::AssistantText {
                text: "Checking files.".into(),
                truncated: false,
            },
            EventKind::ReasoningFinished { item: 2 },
            EventKind::ModelFinished { executing: true },
            EventKind::CellStarted {
                cell: 1,
                text: "var result = 7 * 3 + 2;".into(),
                truncated: false,
            },
            EventKind::CapabilityStarted {
                operation: OperationId(1),
                name: "cocoa.read".into(),
                truncated: false,
            },
            EventKind::CapabilityFinished {
                operation: OperationId(1),
                status: ActivityStatus::Completed,
            },
            EventKind::CellFinished {
                cell: 1,
                status: ActivityStatus::Completed,
                text: String::new(),
                truncated: false,
            },
            EventKind::ModelStarted { round: 1 },
            EventKind::AssistantText {
                text: "Answer preview.".into(),
                truncated: false,
            },
            EventKind::ModelFinished { executing: false },
        ] {
            observe_root_event(&mut chat, kind);
            let view = render(&mut chat, 64, 20)?;
            assert!(view.contains(&format!("╭ {full}{label} ─")), "{view}");
            assert!(view.contains(&format!("{mini} {label}")), "{view}");
            assert_eq!(view.matches(label).count(), 2, "{view}");
            assert_eq!(view.matches(full).count(), 1, "{view}");
            assert_eq!(view.matches(mini).count(), 1, "{view}");
            assert!(!view.contains("Compare the two"), "{view}");
            assert!(!view.contains("Live output incomplete"), "{view}");
        }
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(99),
            kind: EventKind::ReasoningStarted { item: 0 },
        }));
        assert!(render(&mut chat, 64, 20)?.contains(&format!("╭ {full}{label} ─")));
        chat.set_reduced_motion(true);
        let reduced = render(&mut chat, 64, 20)?;
        assert!(reduced.contains("╭  [###] Toasting ─"), "{reduced}");
        assert!(reduced.contains("[#] Toasting ▾"), "{reduced}");
        assert_eq!(reduced.matches("Toasting").count(), 2, "{reduced}");
        assert_eq!(reduced.matches(" [###] ").count(), 1, "{reduced}");
        assert_eq!(reduced.matches("[#]").count(), 1, "{reduced}");
        for elapsed in [
            Duration::from_millis(5760),
            Duration::from_millis(11_520),
            Duration::from_millis(69_120),
            Duration::from_secs(86_400) + Duration::from_millis(600),
            Duration::MAX,
        ] {
            assert!(!chat.tick(elapsed));
            assert_eq!(render(&mut chat, 64, 20)?, reduced);
        }
        chat.complete("Final answer stays visible.");
        let completed = render(&mut chat, 64, 20)?;
        assert!(!completed.contains("●"), "{completed}");
        for label in labels {
            assert!(!completed.contains(label), "{completed}");
        }
        for glyph in [" [###] ", "[#]"] {
            assert!(!completed.contains(glyph), "{completed}");
        }
        assert!(
            completed.contains("Final answer stays visible."),
            "{completed}"
        );
        chat.set_reduced_motion(false);
        type_text(&mut chat, "!");
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("next !λ".into())
        );
        let restarted = render(&mut chat, 64, 20)?;
        assert!(restarted.contains("╭  (o|o) Toasting ─"), "{restarted}");
        assert!(restarted.contains("  (o) Toasting"), "{restarted}");
        assert_eq!(restarted.matches(" (o|o) ").count(), 1, "{restarted}");
        assert_eq!(restarted.matches("(o)").count(), 1, "{restarted}");
    }
    Ok(())
}

#[test]
fn input_notices_stay_above_the_composer_and_out_of_history() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    chat.set_context(".", "high");
    chat.initial_prompt("question")?;
    chat.complete("answer");
    type_text(&mut chat, "next draft");
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    let message = "Alias and model are fixed for this session. Start a new session to change them.";
    chat.input_notice(message);
    chat.input_notice(message);
    for (width, height) in [(96, 16), (32, 16), (32, 9)] {
        let view = render(&mut chat, width, height)?;
        let rows: Vec<_> = view.lines().collect();
        let notice_start = rows
            .iter()
            .position(|row| row.starts_with("  Alias"))
            .ok_or("missing input notice")?;
        let composer_start = rows
            .iter()
            .position(|row| row.starts_with('╭'))
            .ok_or("missing composer")?;
        assert_eq!(
            rows[notice_start..composer_start]
                .iter()
                .map(|row| row.trim())
                .filter(|row| !row.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
            message,
            "{view}"
        );
        assert!(view.contains("next draft"), "{view}");
        assert_eq!(view.matches("Alias and model").count(), 1, "{view}");
    }
    type_text(&mut chat, "!");
    let view = render(&mut chat, 96, 16)?;
    assert!(!view.contains("Alias and model"), "{view}");
    assert!(view.contains("answer"), "{view}");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("next draf!t".into())
    );
    Ok(())
}

#[test]
fn child_provider_pauses_survive_parent_progress_and_completion() -> Result<(), Box<dyn Error>> {
    for width in [100, 40] {
        let mut chat = Chat::new(512)?;
        chat.initial_prompt("prepare licenses")?;
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind: EventKind::ModelStarted { round: 1 },
        }));
        let update = |kind| AgentUpdate {
            session: 1,
            agent: fixture_agent("licenses-cli"),
            name: "licenses-cli".into(),
            kind,
        };
        chat.observe_agent(&update(AgentUpdateKind::Started));
        chat.observe_agent(&update(AgentUpdateKind::Observation(Observation::Event(
            EngineEvent {
                invocation: InvocationId(1),
                kind: EventKind::ProviderRetryScheduled {
                    status: 503,
                    retry_at: std::time::Instant::now() + Duration::from_secs(1),
                },
            },
        ))));
        let retrying = render(&mut chat, width, 24)?;
        assert!(retrying.contains("Retrying"), "{retrying}");
        assert!(!retrying.contains("failed"), "{retrying}");
        chat.observe_agent(&update(AgentUpdateKind::ProviderPaused {
            reason: "HTTP status 503".into(),
        }));
        chat.provider_paused(
            "licenses-cli: HTTP status 503. Retry this request or cancel this agent.",
        );
        chat.input(key(KeyCode::Down, KeyModifiers::NONE));
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind: EventKind::ModelStarted { round: 2 },
        }));
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind: EventKind::AssistantText {
                text: "Parent can continue".into(),
                truncated: false,
            },
        }));
        assert!(!chat.can_submit());
        chat.complete("Parent finished independently");
        let paused = render(&mut chat, width, 24)?;
        assert!(paused.contains("Provider paused"), "{paused}");
        assert!(paused.contains("HTTP status 503"), "{paused}");
        assert!(!paused.contains("failed"), "{paused}");
        assert!(!chat.can_submit());
        chat.provider_paused(
            "licenses-cli: HTTP status 503. Retry this request or cancel this agent.",
        );
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Cancel
        );
        assert_eq!(
            chat.input(key(KeyCode::Char('r'), KeyModifiers::NONE)),
            Action::Retry
        );
        chat.provider_resumed();
        chat.observe_agent(&update(AgentUpdateKind::ProviderResumed));
        chat.observe_agent(&update(AgentUpdateKind::Finished(
            ActivityStatus::Completed,
        )));
        let completed = render(&mut chat, width, 24)?;
        assert!(!completed.contains("Provider paused"), "{completed}");
        assert!(chat.can_submit());
    }
    Ok(())
}

#[test]
fn provider_failures_show_stacked_choices_and_preserve_the_draft() -> Result<(), Box<dyn Error>> {
    for (width, height) in [(96, 24), (36, 18), (20, 12), (20, 8)] {
        let mut chat = Chat::new(256)?;
        chat.initial_prompt("question")?;
        type_text(&mut chat, "next draft");
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind: EventKind::AssistantText {
                text: "unfinished answer".into(),
                truncated: false,
            },
        }));
        chat.provider_paused("Provider returned an invalid response.");
        assert!(!chat.can_submit());
        let view = render(&mut chat, width, height)?;
        let rows: Vec<_> = view.lines().collect();
        let paused = rows
            .iter()
            .position(|row| row.contains("Provider paused"))
            .ok_or("missing paused error")?;
        let retry = rows
            .iter()
            .position(|row| row.contains("Retry"))
            .ok_or("missing retry row")?;
        let cancel = rows
            .iter()
            .position(|row| row.contains("Cancel"))
            .ok_or("missing cancel row")?;
        assert!(paused < retry && retry + 1 == cancel, "{view}");
        assert!(!view.contains("unfinished answer"), "{view}");
        assert!(!view.contains("Options"), "{view}");
        assert_eq!(
            chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Retry
        );
        chat.provider_resumed();
        chat.provider_paused("Provider request failed.");
        assert_eq!(
            chat.input(key(KeyCode::Down, KeyModifiers::NONE)),
            Action::Redraw
        );
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Cancel
        );
        chat.stopping();
        chat.cancelled();
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("next draft".into())
        );
    }
    for (label, expected) in [("Retry", Action::Retry), ("Cancel", Action::Cancel)] {
        let mut chat = Chat::new(256)?;
        chat.initial_prompt("question")?;
        chat.provider_paused("Provider request failed.");
        let view = render(&mut chat, 80, 20)?;
        let (row, text) = view
            .lines()
            .enumerate()
            .find(|(_, row)| row.contains(label))
            .ok_or("missing recovery choice")?;
        let column = text.find(label).ok_or("missing recovery label")?;
        assert_eq!(
            chat.input(mouse(
                MouseEventKind::Down(MouseButton::Left),
                (column as u16, row as u16)
            )),
            expected
        );
    }
    Ok(())
}

#[test]
fn provider_retries_show_a_red_status_and_countdown_without_losing_the_draft()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    chat.set_context(".", "medium");
    chat.initial_prompt("question")?;
    type_text(&mut chat, "next draft");
    for (status, reason) in [
        (429, "Provider Saturation"),
        (503, "Provider Saturation"),
        (500, "Provider Error"),
    ] {
        observe_root_event(&mut chat, EventKind::ModelStarted { round: 1 });
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind: EventKind::ProviderRetryScheduled {
                status,
                retry_at: std::time::Instant::now() + Duration::from_secs(60),
            },
        }));
        for width in [80, 48, 36, 35, 32, 20] {
            let view = render(&mut chat, width, 20)?;
            let rows = view.lines().collect::<Vec<_>>();
            let label_start = rows
                .iter()
                .position(|row| row.starts_with("  Provider"))
                .ok_or("missing inset provider notice")?;
            let composer_start = rows
                .iter()
                .position(|row| row.starts_with('╭'))
                .ok_or("missing composer")?;
            assert!(rows[label_start - 1].is_empty(), "{view}");
            assert!(rows[composer_start - 1].is_empty(), "{view}");
            assert_eq!(
                rows[label_start..composer_start - 1]
                    .iter()
                    .map(|row| row.trim())
                    .collect::<Vec<_>>()
                    .join(" "),
                reason,
                "{view}"
            );
            assert_eq!(view.matches("Retrying").count(), 1, "{view}");
            assert!(
                view.lines()
                    .any(|line| line.starts_with('╭') && line.contains("Retrying in 60s")),
                "{view}"
            );
            assert!(!view.contains("seconds"), "{view}");
            assert!(view.contains("next draft"), "{view}");
            if width >= 48 {
                assert!(view.contains("medium"), "{view}");
                assert!(view.contains("1/3"), "{view}");
            }
            let mut terminal = Terminal::new(TestBackend::new(width, 20))?;
            terminal.draw(|frame| chat.render(frame))?;
            for text in reason.split_whitespace().chain(["Retrying in 60s"]) {
                let cells = terminal
                    .backend()
                    .buffer()
                    .content
                    .windows(text.len())
                    .find(|cells| {
                        cells
                            .iter()
                            .map(ratatui::buffer::Cell::symbol)
                            .collect::<String>()
                            == text
                    })
                    .ok_or("missing retry label")?;
                assert!(
                    cells
                        .iter()
                        .all(|cell| cell.fg == ratatui::style::Color::Rgb(242, 148, 137)),
                    "{text} must be red"
                );
            }
        }
        for width in [80, 20] {
            let view = render(&mut chat, width, 8)?;
            assert!(view.contains("  Provider"), "{view}");
            assert!(
                view.contains(reason.split_whitespace().last().ok_or("empty reason")?),
                "{view}"
            );
            assert!(view.contains("Retrying in 60s"), "{view}");
            assert!(view.contains("next draft"), "{view}");
            let rows = view.lines().collect::<Vec<_>>();
            let composer_start = rows
                .iter()
                .position(|row| row.starts_with('╭'))
                .ok_or("missing compact composer")?;
            assert!(!rows[composer_start - 1].is_empty(), "{view}");
        }
    }
    let now = std::time::Instant::now();
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::ProviderRetryScheduled {
            status: 429,
            retry_at: now + Duration::from_secs(60),
        },
    }));
    assert!(!chat.tick_at(Duration::from_millis(100), now + Duration::from_millis(500)));
    assert!(chat.tick_at(
        Duration::from_millis(200),
        now + Duration::from_millis(1100)
    ));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(99),
        kind: EventKind::ProviderRetryStarted,
    }));
    assert!(render(&mut chat, 80, 20)?.contains("Retrying in"));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::ProviderRetryStarted,
    }));
    let view = render(&mut chat, 80, 20)?;
    assert!(!view.contains("Provider"));
    assert!(view.contains("Toasting"));
    chat.complete("Recovered answer");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("next draft".into())
    );
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(2),
        kind: EventKind::ProviderRetryScheduled {
            status: 503,
            retry_at: std::time::Instant::now() + Duration::from_secs(60),
        },
    }));
    chat.stopping();
    let view = render(&mut chat, 80, 20)?;
    assert!(!view.contains("Retrying"));
    assert!(view.contains("Stopping..."));
    Ok(())
}

#[test]
fn reads_never_disclose_inputs_contents_or_repl_feedback() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "read both");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    let emit = |chat: &mut Chat, kind| {
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind,
        }))
    };
    emit(
        &mut chat,
        EventKind::CellStarted {
            cell: 1,
            text: "var result = 7 * 3 + 2;".into(),
            truncated: false,
        },
    );
    for (operation, title, input) in [
        (OperationId(1), "Read alpha.rs", "Requested lines: 7-9"),
        (OperationId(2), "Read beta.rs", "Requested lines: 17-20"),
    ] {
        emit(
            &mut chat,
            EventKind::CapabilityStarted {
                operation,
                name: "cocoa.read".into(),
                truncated: false,
            },
        );
        emit(
            &mut chat,
            EventKind::CapabilityInput {
                operation,
                title: title.into(),
                text: input.into(),
                summary: None,
                truncated: false,
            },
        );
    }
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let active = render(&mut chat, 64, 32)?;
    assert!(active.contains("Reading alpha.rs"), "{active}");
    assert!(active.contains("Reading beta.rs"), "{active}");
    assert!(!active.contains("Requested lines: 17-20"), "{active}");
    assert!(!active.contains("Requested lines: 7-9"), "{active}");
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(2),
            status: ActivityStatus::Failed { exit_code: None },
        },
    );
    emit(
        &mut chat,
        EventKind::Printed {
            text: "recorded result: 23".into(),
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Completed,
        },
    );
    emit(
        &mut chat,
        EventKind::CellFinished {
            cell: 1,
            status: ActivityStatus::Completed,
            text: "alpha contents; beta unavailable".into(),
            truncated: false,
        },
    );
    let beta = render(&mut chat, 64, 32)?;
    assert!(beta.contains("Read beta.rs"), "{beta}");
    assert!(!beta.contains("Failed"), "{beta}");
    assert!(!beta.contains("Requested lines: 17-20"), "{beta}");
    assert!(!beta.contains("beta unavailable"), "{beta}");
    assert!(!beta.contains("alpha contents"), "{beta}");
    assert!(!beta.contains("recorded result"), "{beta}");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    emit(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Read alpha.rs".into(),
            text: "Requested lines: 7-9".into(),
            summary: None,
            truncated: false,
        },
    );
    let alpha = render(&mut chat, 64, 32)?;
    assert!(alpha.contains("Read alpha.rs"), "{alpha}");
    assert_eq!(alpha.matches("Requested lines: 7-9").count(), 0, "{alpha}");
    assert!(!alpha.contains("alpha contents"), "{alpha}");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let activities = render(&mut chat, 64, 32)?;
    assert!(!activities.contains("var result"), "{activities}");
    assert!(!activities.contains("recorded result"), "{activities}");
    assert!(!activities.contains("JavaScript"), "{activities}");
    assert_eq!(activities.matches('▾').count(), 1, "{activities}");
    assert!(
        activities.lines().any(|line| line.contains("Toasting ▾")),
        "{activities}"
    );
    assert_eq!(activities.matches('▸').count(), 0, "{activities}");
    chat.complete("One read failed; another succeeded.");
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    type_text(&mut chat, "next turn");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(2),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "cocoa.write".into(),
            truncated: false,
        },
    }));
    click_activity(&mut chat, "Work Done")?;
    let next = render(&mut chat, 64, 36)?;
    assert!(next.contains("Read alpha.rs"), "{next}");
    assert!(next.contains("Writing file"), "{next}");
    chat.complete("Unknown write outcome.");
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let settled = render(&mut chat, 64, 36)?;
    assert!(settled.contains("Wrote file · Unknown"), "{settled}");
    assert!(settled.contains("Read beta.rs"), "{settled}");
    Ok(())
}

#[test]
fn waits_are_suppressed_from_activity_while_other_operations_stay_visible()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::capability::{ActivityOutput, ActivityOutputKind};

    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "wait for commands");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    let emit = |chat: &mut Chat, kind| {
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind,
        }))
    };
    emit(
        &mut chat,
        EventKind::CellStarted {
            cell: 1,
            text: "var settled = await cocoa.wait(build);".into(),
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "cocoa.wait".into(),
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Wait commands".into(),
            text: "Commands requested: 1".into(),
            summary: None,
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityOutput {
            operation: OperationId(1),
            output: ActivityOutput {
                text: "captured stdout".into(),
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
            status: ActivityStatus::Failed { exit_code: None },
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(2),
            title: "cocoa.wait".into(),
            text: "Commands requested: 2".into(),
            summary: None,
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(2),
            status: ActivityStatus::Unknown,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityStarted {
            operation: OperationId(3),
            name: "cocoa.read".into(),
            truncated: false,
        },
    );
    emit(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(3),
            status: ActivityStatus::Completed,
        },
    );
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Read file"), "{view}");
    assert!(!view.contains("Wait commands"), "{view}");
    assert!(!view.contains("cocoa.wait"), "{view}");
    assert!(!view.contains("captured stdout"), "{view}");
    assert!(!view.contains("Commands requested"), "{view}");
    chat.complete("Waits stay out of the transcript.");
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    type_text(&mut chat, "next turn");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(2),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "cocoa.write".into(),
            truncated: false,
        },
    }));
    let next = render(&mut chat, 64, 32)?;
    assert!(next.contains("Writing file"), "{next}");
    Ok(())
}

#[test]
fn control_n_and_p_navigate_activities_without_editing_the_draft() -> Result<(), Box<dyn Error>> {
    for phase in ["running", "completed", "stopped"] {
        let mut chat = Chat::new(128)?;
        type_text(&mut chat, "question");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        for (operation, title, text) in [
            (OperationId(1), "First command", "first output"),
            (OperationId(2), "Second command", "second output"),
            (OperationId(3), "Third command", "third output"),
        ] {
            chat.observe(&Observation::Event(EngineEvent {
                invocation: InvocationId(1),
                kind: EventKind::CapabilityInput {
                    operation,
                    title: title.into(),
                    text: text.into(),
                    summary: None,
                    truncated: false,
                },
            }));
        }
        type_text(&mut chat, "next draft");
        chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        match phase {
            "completed" => chat.complete("answer"),
            "stopped" => chat.fail("model provider failed"),
            _ => {}
        }
        if phase != "running" {
            click_activity(&mut chat, "Work Done")?;
        }
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
        for (character, output) in [
            ('n', Some("third output")),
            ('p', Some("second output")),
            ('p', Some("first output")),
            ('p', None),
            ('p', None),
            ('n', Some("first output")),
            ('n', Some("second output")),
            ('n', Some("third output")),
            ('n', Some("third output")),
        ] {
            assert_eq!(
                chat.input(key(KeyCode::Char(character), KeyModifiers::CONTROL)),
                Action::Redraw,
                "{phase}: Ctrl+{character}"
            );
            chat.input(key(KeyCode::Right, KeyModifiers::NONE));
            let view = render(&mut chat, 64, 24)?;
            for other in ["first output", "second output", "third output"] {
                assert_eq!(
                    view.contains(other),
                    Some(other) == output,
                    "{phase}: {view}"
                );
            }
            if output.is_some() {
                chat.input(key(KeyCode::Left, KeyModifiers::NONE));
            }
        }
        chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
        if phase != "stopped" {
            if phase == "running" {
                chat.complete("answer");
            }
            type_text(&mut chat, "!");
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit("next draf!t".into())
            );
        }
    }
    Ok(())
}

#[test]
fn mouse_disclosure_and_stopped_keyboard_navigation_preserve_long_details()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "think");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "Inspect file".into(),
            truncated: false,
        },
    }));
    let collapsed = render(&mut chat, 48, 18)?;
    let row = collapsed
        .lines()
        .position(|line| line.contains("Inspect file"))
        .ok_or("missing activity")? as u16;
    chat.input(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left),
        column: 10,
        row,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(render(&mut chat, 48, 18)?.contains("No details yet."));
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Inspect file".into(),
            text: "safe\x1b\0\r".to_owned() + &"é".repeat(3000) + "\nactivity-tail",
            summary: None,
            truncated: false,
        },
    }));
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    chat.fail("model provider failed");
    click_activity(&mut chat, "Work Done")?;
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    for (width, height) in [(64, 24), (16, 12), (4, 6), (1, 2)] {
        let view = render(&mut chat, width, height)?;
        assert!(
            !view
                .chars()
                .any(|character| character.is_control() && character != '\n')
        );
    }
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    let stopped = render(&mut chat, 64, 20)?;
    assert!(stopped.contains("Inspect file · Unknown"), "{stopped}");
    assert!(!stopped.contains("safeé"), "{stopped}");
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    for _ in 0..100 {
        chat.input(key(KeyCode::PageDown, KeyModifiers::NONE));
    }
    let details = render(&mut chat, 64, 20)?;
    assert!(details.contains("activity-tail"), "{details}");
    assert!(!details.contains("[Details omitted]"), "{details}");
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityOutput {
            operation: OperationId(1),
            output: cocoa_core::capability::ActivityOutput {
                text: "upstream preview".into(),
                kind: cocoa_core::capability::ActivityOutputKind::Command,
                summary: None,
                truncated: true,
            },
        },
    }));
    let details = render(&mut chat, 64, 20)?;
    assert!(details.contains("upstream preview"), "{details}");
    assert!(!details.contains("[Details omitted]"), "{details}");
    Ok(())
}

/// Output arrives between frames while an agent streams; the header the user
/// sees on screen must still toggle when clicked.
#[test]
fn streaming_output_keeps_activity_headers_clickable() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "think");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "Inspect file".into(),
            truncated: false,
        },
    }));
    let collapsed = render(&mut chat, 64, 24)?;
    let row = collapsed
        .lines()
        .position(|line| line.contains("Inspect file"))
        .ok_or("missing activity")? as u16;
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityOutput {
            operation: OperationId(1),
            output: cocoa_core::capability::ActivityOutput {
                text: "streamed detail".into(),
                kind: cocoa_core::capability::ActivityOutputKind::Command,
                summary: None,
                truncated: false,
            },
        },
    }));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (10, row))),
        Action::Redraw
    );
    assert!(render(&mut chat, 64, 24)?.contains("streamed detail"));
    Ok(())
}

#[test]
fn mouse_interactions_keep_the_draft_editable_at_its_cursor() -> Result<(), Box<dyn Error>> {
    for running in [false, true] {
        for navigate_first in [false, true] {
            for interaction in [
                "activity",
                "transcript",
                "padding",
                "scroll up",
                "scroll down",
            ] {
                let mut chat = Chat::new(128)?;
                type_text(&mut chat, "question");
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
                chat.observe(&Observation::Event(EngineEvent {
                    invocation: InvocationId(1),
                    kind: EventKind::CapabilityStarted {
                        operation: OperationId(1),
                        name: "Inspect file".into(),
                        truncated: false,
                    },
                }));
                chat.observe(&Observation::Event(EngineEvent {
                    invocation: InvocationId(1),
                    kind: EventKind::CapabilityInput {
                        operation: OperationId(1),
                        title: "Inspect file".into(),
                        text: "Keep these details visible.".into(),
                        summary: None,
                        truncated: false,
                    },
                }));
                if !running {
                    chat.observe(&Observation::Event(EngineEvent {
                        invocation: InvocationId(1),
                        kind: EventKind::CapabilityFinished {
                            operation: OperationId(1),
                            status: ActivityStatus::Completed,
                        },
                    }));
                    chat.complete("answer");
                    click_activity(&mut chat, "Work Done")?;
                }
                type_text(&mut chat, "next\ncafé λ");
                chat.input(key(KeyCode::Left, KeyModifiers::NONE));
                if navigate_first {
                    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
                }
                let view = render(&mut chat, 64, 20)?;
                let activity_row = view
                    .lines()
                    .position(|line| line.contains("Inspect file"))
                    .ok_or("missing activity")? as u16;
                let (kind, position) = match interaction {
                    "activity" => (MouseEventKind::Down(MouseButton::Left), (10, activity_row)),
                    "transcript" => (
                        MouseEventKind::Down(MouseButton::Left),
                        (10, activity_row + 1),
                    ),
                    "padding" => (MouseEventKind::Down(MouseButton::Left), (0, 0)),
                    "scroll up" => (MouseEventKind::ScrollUp, (10, activity_row)),
                    _ => (MouseEventKind::ScrollDown, (10, activity_row)),
                };
                chat.input(mouse(kind, position));
                if matches!(kind, MouseEventKind::Down(_)) {
                    chat.input(mouse(MouseEventKind::Up(MouseButton::Left), position));
                }
                if interaction == "activity" {
                    let expanded = render(&mut chat, 64, 20)?;
                    assert!(
                        expanded.contains("Keep these details visible."),
                        "{expanded}"
                    );
                }
                assert_eq!(
                    chat.input(key(KeyCode::Char('!'), KeyModifiers::NONE)),
                    Action::Redraw,
                    "{interaction}, running={running}, navigate_first={navigate_first}"
                );
                let edited = render(&mut chat, 64, 20)?;
                assert!(edited.contains("café !λ"), "{interaction}: {edited}");
                assert_eq!(
                    chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                    if running {
                        Action::Steer("next\ncafé !λ".into())
                    } else {
                        Action::Submit("next\ncafé !λ".into())
                    }
                );
            }
        }
    }
    Ok(())
}

#[test]
fn composer_click_moves_the_cursor_to_the_clicked_row_and_column() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    type_text(&mut chat, "first line\nsecond line");

    let view = render(&mut chat, 40, 12)?;
    let row = view
        .lines()
        .position(|line| line.contains("second line"))
        .ok_or("missing second draft line")?;
    let line = view.lines().nth(row).ok_or("missing second draft line")?;
    let start = line
        .find("second line")
        .ok_or("missing second draft line column")?;
    let column = line[..start].chars().count() + "sec".len();
    let position = (u16::try_from(column)?, u16::try_from(row)?);
    assert_eq!(
        chat.input(mouse(MouseEventKind::Down(MouseButton::Left), position)),
        Action::Redraw
    );
    chat.input(mouse(MouseEventKind::Up(MouseButton::Left), position));
    type_text(&mut chat, "!");
    let edited = render(&mut chat, 40, 12)?;
    assert!(
        edited.contains("sec!ond line"),
        "click did not move the cursor into the second line: {edited}"
    );

    let view = render(&mut chat, 40, 12)?;
    let row = view
        .lines()
        .position(|line| line.contains("first line"))
        .ok_or("missing first draft line")?;
    let line = view.lines().nth(row).ok_or("missing first draft line")?;
    let start = line
        .find("first line")
        .ok_or("missing first draft line column")?;
    let column = line[..start].chars().count() + "fir".len();
    let position = (u16::try_from(column)?, u16::try_from(row)?);
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), position));
    chat.input(mouse(MouseEventKind::Up(MouseButton::Left), position));
    type_text(&mut chat, "?");
    let edited = render(&mut chat, 40, 12)?;
    assert!(
        edited.contains("fir?st line"),
        "click did not move the cursor into the first line: {edited}"
    );
    Ok(())
}

#[test]
fn arrows_recall_the_last_prompt_and_return_to_empty_without_overwriting_edits()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(32)?;
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    for prompt in ["hi!", "café\nλ"] {
        type_text(&mut chat, prompt);
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(prompt.into())
        );
        chat.complete("reply");
        for _ in 0..2 {
            chat.input(key(KeyCode::Up, KeyModifiers::NONE));
            let view = render(&mut chat, 64, 16)?;
            assert!(
                view.contains(&format!(
                    "│  {}",
                    prompt.lines().next().ok_or("empty prompt")?
                )),
                "{view}"
            );
            chat.input(key(KeyCode::Up, KeyModifiers::NONE));
            chat.input(key(KeyCode::Down, KeyModifiers::NONE));
            chat.input(key(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::None
            );
        }
        chat.input(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(prompt.into())
        );
        chat.complete("reply");
    }
    type_text(&mut chat, "my draft");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("my draft".into())
    );
    chat.complete("reply");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    type_text(&mut chat, " edited");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("my draft edited".into())
    );
    Ok(())
}

#[test]
fn double_up_recalls_and_down_restores_the_draft_and_cursor() -> Result<(), Box<dyn Error>> {
    for running in [false, true] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "previous message");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        if !running {
            chat.complete("reply");
        }
        type_text(&mut chat, "café λ draft");
        chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        chat.input(key(KeyCode::Up, KeyModifiers::NONE));
        let draft = render(&mut chat, 64, 16)?;
        assert!(draft.contains("│  café λ draft"), "{draft}");
        chat.input(key(KeyCode::Up, KeyModifiers::NONE));
        let recalled = render(&mut chat, 64, 16)?;
        assert!(recalled.contains("│  previous message"), "{recalled}");
        assert!(!recalled.contains("│  café λ draft"), "{recalled}");
        chat.input(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(render(&mut chat, 64, 16)?, draft);
        type_text(&mut chat, "!");
        if running {
            chat.complete("reply");
        }
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("café λ draf!t".into())
        );
    }
    Ok(())
}

#[test]
fn prompt_recall_keeps_multiline_navigation_and_requires_consecutive_up_presses()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "previous");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("reply");
    type_text(&mut chat, "first\nsecond\nthird");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    let draft = render(&mut chat, 64, 16)?;
    assert!(draft.contains("│  first"), "{draft}");
    assert!(draft.contains("│  third"), "{draft}");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert!(render(&mut chat, 64, 16)?.contains("│  previous"));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(render(&mut chat, 64, 16)?, draft);
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(render(&mut chat, 64, 16)?, draft);
    chat.input(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    )));
    assert_eq!(render(&mut chat, 64, 16)?, draft);
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("first\nsecond\nthird".into())
    );
    chat.complete("reply");
    let wrapped = "abcdefghijklmnopqrSTUVWXYZ0123456789!lastrow";
    type_text(&mut chat, wrapped);
    render(&mut chat, 24, 16)?;
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    render(&mut chat, 24, 16)?;
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(wrapped.into())
    );
    Ok(())
}

#[test]
fn prompt_recall_preserves_full_input_and_busy_drafts() -> Result<(), Box<dyn Error>> {
    let prompt = "é".repeat(20000);
    let mut chat = Chat::new(prompt.len())?;
    chat.set_message_steering(false);
    type_text(&mut chat, &prompt);
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(prompt.clone().into())
    );
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    chat.complete("reply");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(prompt.into())
    );
    type_text(&mut chat, "next draft");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.complete("reply");
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("next draft".into())
    );
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut chat, "keep");
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.complete("reply");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("keep".into())
    );
    chat.fail("stopped");
    let stopped = render(&mut chat, 64, 16)?;
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(render(&mut chat, 64, 16)?, stopped);
    Ok(())
}

#[test]
fn file_mentions_filter_navigate_and_submit_only_path_text() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("src"))?;
    std::fs::write(directory.path().join("alpha.rs"), "DO NOT ATTACH THIS BODY")?;
    std::fs::write(directory.path().join("src/lib.rs"), [0xff, 0, 0x1b])?;
    let mut chat = Chat::new(128)?;
    chat.set_working_directory(directory.path());
    type_text(&mut chat, "previous message");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("reply");
    type_text(&mut chat, "Explain @");
    settle_files(&mut chat)?;
    let view = render(&mut chat, 64, 16)?;
    assert!(view.contains("› alpha.rs"), "{view}");
    assert!(view.contains("src/lib.rs"), "{view}");
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(render(&mut chat, 64, 16)?, view);
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert!(!render(&mut chat, 64, 16)?.contains(" Files "));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("Explain @src/ ".into())
    );

    // Completion also works on the next draft while the invocation is running.
    type_text(&mut chat, "Read @SRcLb");
    settle_files(&mut chat)?;
    assert!(render(&mut chat, 64, 16)?.contains("› src/lib.rs"));
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Steer("Read @src/lib.rs ".into())
    );
    chat.complete("done");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn control_n_and_p_navigate_file_mentions_without_editing_the_draft() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    for index in 0..9 {
        std::fs::write(directory.path().join(format!("file-{index}.rs")), "body")?;
    }
    let mut chat = Chat::new(128)?;
    chat.set_working_directory(directory.path());
    type_text(&mut chat, "Explain @");
    settle_files(&mut chat)?;
    chat.input(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(render(&mut chat, 64, 16)?.contains("› file-0.rs"));
    for index in 1..9 {
        chat.input(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        let view = render(&mut chat, 64, 16)?;
        assert!(view.contains(&format!("› file-{index}.rs")), "{view}");
    }
    chat.input(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(render(&mut chat, 64, 16)?.contains("› file-8.rs"));
    chat.input(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('p'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    )));
    assert!(render(&mut chat, 64, 16)?.contains("› file-7.rs"));
    for (character, modifiers, kind) in [
        ('p', KeyModifiers::CONTROL, KeyEventKind::Release),
        (
            'p',
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Press,
        ),
        (
            'n',
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ),
        ('p', KeyModifiers::SUPER, KeyEventKind::Press),
    ] {
        chat.input(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char(character),
            modifiers,
            kind,
        )));
        assert!(render(&mut chat, 64, 16)?.contains("› file-7.rs"));
    }
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("Explain @file-7.rs ".into())
    );
    chat.complete("answer");
    type_text(&mut chat, "@missing");
    settle_files(&mut chat)?;
    for character in ['n', 'p'] {
        chat.input(key(KeyCode::Char(character), KeyModifiers::CONTROL));
        assert!(render(&mut chat, 64, 16)?.contains("No matching paths"));
    }
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("@missing".into())
    );
    chat.complete("answer");
    type_text(&mut chat, "draft");
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    for character in ['n', 'p'] {
        chat.input(key(KeyCode::Char(character), KeyModifiers::CONTROL));
    }
    type_text(&mut chat, "!");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("draf!t".into())
    );
    Ok(())
}

#[test]
fn file_mentions_preserve_unicode_surrounding_text_and_quoted_directories()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir(directory.path().join("café notes"))?;
    std::fs::create_dir_all(directory.path().join("outside/café notes"))?;
    std::fs::write(
        directory.path().join("outside/café notes/λ.txt"),
        "outside contents",
    )?;
    std::fs::write(
        directory.path().join("café notes/λ.txt"),
        "private contents",
    )?;
    std::fs::write(directory.path().join("other.rs"), "other contents")?;
    let mut chat = Chat::new(256)?;
    chat.set_working_directory(directory.path());
    type_text(&mut chat, "λ first\nRead @caf");
    settle_files(&mut chat)?;
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    type_text(&mut chat, "λ");
    settle_files(&mut chat)?;
    let view = render(&mut chat, 64, 16)?;
    assert!(view.contains("› café notes/λ.txt"), "{view}");
    assert!(!view.contains("outside"), "{view}");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    type_text(&mut chat, "and @otBAD after");
    for _ in 0..9 {
        chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    }
    settle_files(&mut chat)?;
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.input(key(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("λ first\nRead @\"café notes/λ.txt\" and @other.rs after".into())
    );
    for prefix in ["🦀".repeat(73), "é界e\u{301}".repeat(500)] {
        let mut chat = Chat::new(8192)?;
        chat.set_working_directory(directory.path());
        chat.paste_text(&format!("{prefix} @ot after"))?;
        for _ in 0..6 {
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        }
        settle_files(&mut chat)?;
        assert!(render(&mut chat, 64, 16)?.contains("› other.rs"));
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(format!("{prefix} @other.rs after").into())
        );
    }
    Ok(())
}

#[test]
fn file_mentions_respect_discovery_and_input_bounds_without_expanding_raw_paths()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join(".gitignore"), "ignored/\n")?;
    std::fs::create_dir(directory.path().join("ignored"))?;
    std::fs::write(directory.path().join("ignored/secret.txt"), "secret")?;
    std::fs::write(directory.path().join(".hidden"), "hidden")?;
    std::fs::write(directory.path().join("long-name.txt"), "body")?;
    for (budget, expected) in [
        (16, "@long-name.txt "),
        (15, "@long-name.txt "),
        (14, "@lo"),
    ] {
        let mut chat = Chat::new(budget)?;
        chat.set_working_directory(directory.path());
        type_text(&mut chat, "@lo");
        settle_files(&mut chat)?;
        let view = render(&mut chat, 64, 16)?;
        assert!(view.contains("› long-name.txt"), "{view}");
        assert!(!view.contains("secret.txt"), "{view}");
        chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
        chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(expected.into())
        );
    }
    let mut chat = Chat::new(128)?;
    chat.set_working_directory(directory.path());
    type_text(&mut chat, "user@example.com");
    chat.tick(Duration::from_millis(40));
    assert!(!render(&mut chat, 64, 16)?.contains(" Files "));
    type_text(&mut chat, " @");
    settle_files(&mut chat)?;
    let view = render(&mut chat, 64, 16)?;
    assert!(!view.contains("ignored"), "{view}");
    assert!(!view.contains("hidden"), "{view}");
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    chat.tick(Duration::from_millis(40));
    assert!(!render(&mut chat, 64, 16)?.contains(" Files "));
    type_text(&mut chat, "not-present");
    settle_files(&mut chat)?;
    assert!(render(&mut chat, 64, 16)?.contains("No matching paths"));
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("user@example.com @not-present".into())
    );
    for (suffix, offered) in [("", true), ("x", false)] {
        let mut chat = Chat::new(8192)?;
        chat.set_working_directory(directory.path());
        let prompt = format!("@{}{suffix}", "λ".repeat(2048));
        chat.paste_text(&prompt)?;
        chat.tick(Duration::from_millis(40));
        assert_eq!(render(&mut chat, 64, 16)?.contains(" Files "), offered);
        chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(prompt.into())
        );
    }
    Ok(())
}

#[test]
fn modified_arrows_move_to_draft_edges_and_word_boundaries() -> Result<(), Box<dyn Error>> {
    for running in [false, true] {
        for (code, modifiers, expected) in [
            (KeyCode::Left, KeyModifiers::SUPER, "|first\ncafé  tail"),
            (KeyCode::Right, KeyModifiers::SUPER, "first\ncafé  tail|"),
            (KeyCode::Left, KeyModifiers::ALT, "first\ncafé  |tail"),
            (KeyCode::Right, KeyModifiers::ALT, "first\ncafé  tail|"),
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                "|first\ncafé  tail",
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                "first\ncafé  tail|",
            ),
            (KeyCode::Char('b'), KeyModifiers::ALT, "first\ncafé  |tail"),
            (KeyCode::Char('f'), KeyModifiers::ALT, "first\ncafé  tail|"),
        ] {
            let mut chat = Chat::new(64)?;
            if running {
                type_text(&mut chat, "previous");
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            }
            type_text(&mut chat, "first\ncafé  tail");
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
            assert_eq!(chat.input(key(code, modifiers)), Action::Redraw);
            type_text(&mut chat, "|");
            if running {
                chat.complete("reply");
            }
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit(expected.into()),
                "{code:?} {modifiers:?}, running={running}"
            );
        }
    }

    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "café  tail");
    for _ in 0..3 {
        chat.input(key(KeyCode::Left, KeyModifiers::ALT));
    }
    type_text(&mut chat, "|");
    chat.input(key(KeyCode::Right, KeyModifiers::ALT));
    type_text(&mut chat, "|");
    chat.input(key(KeyCode::Right, KeyModifiers::ALT));
    type_text(&mut chat, "|");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("|café  |tail|".into())
    );
    Ok(())
}

#[test]
fn option_delete_removes_the_previous_word_without_changing_other_edits()
-> Result<(), Box<dyn Error>> {
    for (code, modifiers) in [
        (KeyCode::Backspace, KeyModifiers::ALT),
        (KeyCode::Char('\u{8}'), KeyModifiers::ALT),
        (
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ),
        (KeyCode::Char('w'), KeyModifiers::CONTROL),
    ] {
        for (draft, expected) in [
            ("keep café λambda", "keep café "),
            ("keep café   ", "keep "),
            ("keep café!", "keep café"),
        ] {
            let mut chat = Chat::new(draft.len())?;
            for character in draft.chars() {
                chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
            }
            chat.input(Event::Key(KeyEvent::new_with_kind(
                code,
                modifiers,
                KeyEventKind::Release,
            )));
            chat.input(key(code, modifiers));
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit(expected.into()),
                "{code:?} {modifiers:?} on {draft:?}"
            );
            assert_eq!(chat.input(key(code, modifiers)), Action::None);
            chat.fail("stopped");
            assert_eq!(chat.input(key(code, modifiers)), Action::None);
        }
        let mut chat = Chat::new(64)?;
        for character in "keep café tail".chars() {
            chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
        }
        for _ in 0..5 {
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        }
        chat.input(key(code, modifiers));
        chat.input(key(KeyCode::Enter, KeyModifiers::CONTROL));
        chat.input(key(code, modifiers));
        chat.input(key(KeyCode::End, KeyModifiers::NONE));
        chat.input(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("keep  tai".into())
        );
    }
    Ok(())
}

#[test]
fn command_backspace_clears_the_entire_draft_while_idle_or_running() -> Result<(), Box<dyn Error>> {
    for (code, modifiers) in [
        (KeyCode::Backspace, KeyModifiers::SUPER),
        (KeyCode::Char('\u{8}'), KeyModifiers::SUPER),
        (KeyCode::Char('u'), KeyModifiers::CONTROL),
    ] {
        for running in [false, true] {
            let mut chat = Chat::new(64)?;
            for character in "previous prompt".chars() {
                chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
            }
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            if !running {
                chat.complete("previous reply");
            }
            for character in "first café\nsecond λ tail".chars() {
                let event = if character == '\n' {
                    key(KeyCode::Enter, KeyModifiers::CONTROL)
                } else {
                    key(KeyCode::Char(character), KeyModifiers::NONE)
                };
                chat.input(event);
            }
            chat.input(key(KeyCode::Up, KeyModifiers::NONE));
            chat.input(key(KeyCode::Home, KeyModifiers::NONE));
            let before = render(&mut chat, 64, 16)?;
            assert_eq!(
                chat.input(Event::Key(KeyEvent::new_with_kind(
                    code,
                    modifiers,
                    KeyEventKind::Release,
                ))),
                Action::None
            );
            assert_eq!(render(&mut chat, 64, 16)?, before);
            assert_eq!(chat.input(key(code, modifiers)), Action::Redraw);
            assert_eq!(chat.input(key(code, modifiers)), Action::None);
            let cleared = render(&mut chat, 64, 16)?;
            assert!(cleared.contains("previous prompt"), "{cleared}");
            assert!(!cleared.contains("first café"), "{cleared}");
            assert!(!cleared.contains("second λ tail"), "{cleared}");
            if running {
                chat.complete("previous reply");
            }
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::None
            );
            chat.input(key(KeyCode::Char('é'), KeyModifiers::NONE));
            chat.input(key(KeyCode::Enter, KeyModifiers::CONTROL));
            chat.input(key(KeyCode::Char('🦀'), KeyModifiers::NONE));
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit("é\n🦀".into())
            );
            chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE));
            chat.fail("stopped");
            let stopped = render(&mut chat, 64, 16)?;
            assert_eq!(chat.input(key(code, modifiers)), Action::None);
            assert_eq!(render(&mut chat, 64, 16)?, stopped);
        }
    }
    Ok(())
}

#[test]
fn copy_confirmation_expires_and_repeated_copies_refresh_it_without_moving_content()
-> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    for phase in ["idle", "running", "stopping", "stopped"] {
        for width in [12, 24, 64] {
            let mut chat = Chat::new(64)?;
            chat.set_context("/project", "medium");
            if phase != "idle" {
                type_text(&mut chat, "question");
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            }
            type_text(&mut chat, "draft intact");
            if phase == "stopping" {
                chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
                chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
            } else if phase == "stopped" {
                chat.fail("Stopped by fixture");
            }
            let before = render(&mut chat, width, 16)?;
            assert!(!before.contains("Copied!"));
            chat.confirm_copy(now);
            let copied = render(&mut chat, width, 16)?;
            let notice = format!("{:>width$}", " Copied! ", width = usize::from(width));
            assert_eq!(copied.lines().next(), Some(notice.trim_end()));
            for (before, after) in before.lines().skip(1).zip(copied.lines().skip(1)) {
                assert_eq!(before, after);
            }
            chat.tick_at(Duration::ZERO, now + Duration::from_millis(1499));
            assert!(render(&mut chat, width, 16)?.contains("Copied!"));
            assert!(chat.tick_at(Duration::ZERO, now + Duration::from_millis(1500)));
            assert_eq!(render(&mut chat, width, 16)?, before);
            chat.confirm_copy(now + Duration::from_secs(2));
            chat.confirm_copy(now + Duration::from_secs(3));
            chat.tick_at(Duration::ZERO, now + Duration::from_millis(3500));
            assert!(render(&mut chat, width, 16)?.contains("Copied!"));
            chat.tick_at(Duration::ZERO, now + Duration::from_millis(4499));
            assert!(render(&mut chat, width, 16)?.contains("Copied!"));
            assert!(chat.tick_at(Duration::ZERO, now + Duration::from_millis(4500)));
            assert_eq!(render(&mut chat, width, 16)?, before);
        }
    }
    Ok(())
}

#[test]
fn drag_copies_logical_text_across_wraps_and_messages_in_both_directions()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "Ask");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("alpha beta gamma delta\nsecond line");
    let view = render(&mut chat, 18, 20)?;
    assert!(
        view.lines()
            .nth(4)
            .is_some_and(|line| line == "  alpha beta"),
        "{view}"
    );
    assert!(
        view.lines()
            .nth(6)
            .is_some_and(|line| line == "  second line"),
        "{view}"
    );
    for (start, end) in [((2, 4), (12, 6)), ((12, 6), (2, 4))] {
        assert_eq!(
            chat.input(mouse(MouseEventKind::Down(MouseButton::Left), start)),
            Action::Redraw
        );
        assert_eq!(
            chat.input(mouse(MouseEventKind::Drag(MouseButton::Left), end)),
            Action::Redraw
        );
        render(&mut chat, 18, 20)?;
        assert_eq!(
            chat.input(mouse(MouseEventKind::Up(MouseButton::Left), end)),
            Action::Copy("alpha beta gamma delta\nsecond line".into())
        );
        assert_eq!(
            chat.input(mouse(MouseEventKind::Up(MouseButton::Left), end)),
            Action::None
        );
    }
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (4, 1)));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (6, 4))),
        Action::Copy("Ask\n\nalpha".into())
    );
    Ok(())
}

#[test]
fn drag_selects_whole_unicode_graphemes_and_leaves_the_draft_unchanged()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "Ask");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("ab界e\u{301}🦀 end");
    type_text(&mut chat, "ab界e\u{301}🦀 end");
    let view = render(&mut chat, 32, 20)?;
    assert!(
        view.lines()
            .nth(4)
            .is_some_and(|line| line.starts_with("  ab界") && line.contains("e\u{301}🦀")),
        "{view}"
    );
    let composer_row = view
        .lines()
        .position(|line| line.contains("│  ab界"))
        .ok_or("missing draft")? as u16;
    for (start, end) in [((8, 4), (5, 4)), ((9, composer_row), (6, composer_row))] {
        chat.input(mouse(MouseEventKind::Down(MouseButton::Left), start));
        chat.input(mouse(MouseEventKind::Drag(MouseButton::Left), end));
        let mut terminal = Terminal::new(TestBackend::new(32, 20))?;
        terminal.draw(|frame| chat.render(frame))?;
        assert_ne!(
            terminal.backend().buffer()[(end.0 - 1, end.1)].bg,
            ratatui::style::Color::Reset
        );
        assert_eq!(
            chat.input(mouse(MouseEventKind::Up(MouseButton::Left), end)),
            Action::Copy("界e\u{301}🦀".into())
        );
    }
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("ab界e\u{301}🦀 end".into())
    );
    let answer = "A界e\u{301}🦀👩‍💻 mixed 界 words Z";
    chat.complete(answer);
    let view = render(&mut chat, 6, 64)?;
    let start_row = view
        .lines()
        .position(|line| line.contains('A'))
        .ok_or("missing start")? as u16;
    let end_row = view
        .lines()
        .position(|line| line.contains('Z'))
        .ok_or("missing end")? as u16;
    chat.input(mouse(
        MouseEventKind::Down(MouseButton::Left),
        (2, start_row),
    ));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (3, end_row))),
        Action::Copy(answer.into())
    );
    Ok(())
}

#[test]
fn double_click_copies_unicode_words_and_emoji_in_history_and_the_draft()
-> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    let text = "prefix café_e\u{301}, 👩‍💻 suffix";
    for composer in [false, true] {
        for (symbol, continuation, expected) in
            [("é", 0, "café_e\u{301}"), (",", 0, ","), ("👩‍💻", 1, "👩‍💻")]
        {
            let mut chat = Chat::new(128)?;
            type_text(&mut chat, "Ask");
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            chat.complete(text);
            type_text(&mut chat, text);
            let view = render(&mut chat, 64, 20)?;
            let row = if composer {
                view.lines()
                    .position(|line| line.starts_with("│  prefix"))
                    .ok_or("missing draft")? as u16
            } else {
                4
            };
            let mut terminal = Terminal::new(TestBackend::new(64, 20))?;
            terminal.draw(|frame| chat.render(frame))?;
            let column = (0..64)
                .find(|&column| terminal.backend().buffer()[(column, row)].symbol() == symbol)
                .ok_or("missing target glyph")?
                + continuation;
            let position = (column, row);
            chat.input_at(
                mouse(MouseEventKind::Down(MouseButton::Left), position),
                now,
            );
            assert_eq!(
                chat.input_at(mouse(MouseEventKind::Up(MouseButton::Left), position), now),
                Action::Redraw
            );
            terminal.draw(|frame| chat.render(frame))?;
            let second = now + Duration::from_millis(200);
            chat.input_at(
                mouse(MouseEventKind::Down(MouseButton::Left), position),
                second,
            );
            terminal.draw(|frame| chat.render(frame))?;
            assert_ne!(
                terminal.backend().buffer()[(column - continuation, row)].bg,
                ratatui::style::Color::Reset
            );
            assert_eq!(
                chat.input_at(
                    mouse(MouseEventKind::Up(MouseButton::Left), position),
                    second
                ),
                Action::Copy(expected.into()),
                "composer={composer}, glyph={symbol}, position={position:?}"
            );
            assert_eq!(
                chat.input_at(
                    mouse(MouseEventKind::Up(MouseButton::Left), position),
                    second
                ),
                Action::None
            );
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit(text.into())
            );
        }
    }
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "Ask");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("extraordinary word");
    let view = render(&mut chat, 12, 20)?;
    let row = view
        .lines()
        .position(|line| line.contains("inary"))
        .ok_or("missing wrapped word")? as u16;
    for instant in [now, now + Duration::from_millis(200)] {
        chat.input_at(
            mouse(MouseEventKind::Down(MouseButton::Left), (2, row)),
            instant,
        );
        let action = chat.input_at(
            mouse(MouseEventKind::Up(MouseButton::Left), (2, row)),
            instant,
        );
        if instant != now {
            assert_eq!(action, Action::Copy("extraordinary".into()));
        }
    }
    Ok(())
}

#[test]
fn double_click_requires_a_recent_uninterrupted_click_on_the_same_text_cell()
-> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    for (first, second, delay, expected) in [
        ((3, 4), (3, 4), 399, Action::Copy("alpha".into())),
        ((3, 4), (3, 4), 400, Action::Redraw),
        ((3, 4), (3, 4), 401, Action::Redraw),
        ((3, 4), (4, 4), 100, Action::Redraw),
        ((7, 4), (7, 4), 100, Action::Redraw),
        ((20, 4), (20, 4), 100, Action::Redraw),
    ] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "Ask");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        chat.complete("alpha beta");
        render(&mut chat, 32, 20)?;
        chat.input_at(mouse(MouseEventKind::Down(MouseButton::Left), first), now);
        assert_eq!(
            chat.input_at(mouse(MouseEventKind::Up(MouseButton::Left), first), now),
            Action::Redraw
        );
        let instant = now + Duration::from_millis(delay);
        chat.input_at(
            mouse(MouseEventKind::Down(MouseButton::Left), second),
            instant,
        );
        assert_eq!(
            chat.input_at(
                mouse(MouseEventKind::Up(MouseButton::Left), second),
                instant
            ),
            expected
        );
    }
    for event in [
        Event::Resize(32, 20),
        Event::FocusLost,
        mouse(MouseEventKind::ScrollUp, (3, 4)),
        key(KeyCode::Char('x'), KeyModifiers::NONE),
    ] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "Ask");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        chat.complete("alpha beta");
        render(&mut chat, 32, 20)?;
        chat.input_at(mouse(MouseEventKind::Down(MouseButton::Left), (3, 4)), now);
        chat.input_at(mouse(MouseEventKind::Up(MouseButton::Left), (3, 4)), now);
        chat.input_at(event, now);
        render(&mut chat, 32, 20)?;
        let instant = now + Duration::from_millis(100);
        chat.input_at(
            mouse(MouseEventKind::Down(MouseButton::Left), (3, 4)),
            instant,
        );
        assert_eq!(
            chat.input_at(
                mouse(MouseEventKind::Up(MouseButton::Left), (3, 4)),
                instant
            ),
            Action::Redraw
        );
    }
    Ok(())
}

#[test]
fn triple_click_copies_one_logical_paragraph_without_adjacent_paragraphs()
-> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    let text = "Before.\n\nFirst café line\nsecond target line\n \nAfter.";
    for composer in [false, true] {
        for (width, whitespace) in [(32, false), (32, true), (16, false)] {
            let mut chat = Chat::new(128)?;
            type_text(&mut chat, text);
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            chat.complete("done");
            type_text(&mut chat, text);
            let view = render(&mut chat, width, 32)?;
            let (row, line) = view
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains("target") && line.starts_with('│') == composer)
                .ok_or("missing target paragraph")?;
            let prefix = line.split_once("target").ok_or("missing word")?.0;
            let column = prefix.chars().count() as u16 + if whitespace { 0 } else { 2 } - 1;
            let position = (column, row as u16);
            for count in 0..6 {
                let instant = now + Duration::from_millis(count * 100);
                chat.input_at(
                    mouse(MouseEventKind::Down(MouseButton::Left), position),
                    instant,
                );
                let action = chat.input_at(
                    mouse(MouseEventKind::Up(MouseButton::Left), position),
                    instant,
                );
                assert_eq!(
                    action,
                    match count % 3 {
                        1 if !whitespace => Action::Copy("target".into()),
                        2 => Action::Copy("First café line\nsecond target line".into()),
                        _ => Action::Redraw,
                    }
                );
                if matches!(action, Action::Copy(_)) {
                    chat.confirm_copy(instant);
                }
                render(&mut chat, width, 32)?;
            }
        }
    }
    Ok(())
}

#[test]
fn clicks_and_invalidated_drags_do_not_copy() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "Ask");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("alpha beta");
    render(&mut chat, 32, 20)?;
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (2, 4)));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (2, 4))),
        Action::Redraw
    );
    for event in [
        Event::Resize(24, 16),
        Event::FocusLost,
        mouse(MouseEventKind::ScrollUp, (2, 4)),
        key(KeyCode::Char('x'), KeyModifiers::NONE),
    ] {
        render(&mut chat, 32, 20)?;
        chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (2, 4)));
        chat.input(mouse(MouseEventKind::Drag(MouseButton::Left), (6, 4)));
        chat.input(event);
        assert_eq!(
            chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (6, 4))),
            Action::None
        );
    }
    render(&mut chat, 32, 20)?;
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (0, 4)));
    assert_eq!(
        chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (6, 4))),
        Action::None
    );
    Ok(())
}

#[test]
fn selection_uses_scrolled_rows_and_cancels_when_streamed_text_changes()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{Event as EngineEvent, EventKind, Observation};
    let answer = (0..30)
        .map(|index| format!("line-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    for stopped in [false, true] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "Ask");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        if stopped {
            chat.fail(&answer);
        } else {
            chat.observe(&Observation::Event(EngineEvent {
                invocation: cocoa_core::store::InvocationId(1),
                kind: EventKind::AssistantText {
                    text: answer.clone(),
                    truncated: false,
                },
            }));
        }
        chat.input(mouse(MouseEventKind::ScrollUp, (2, 0)));
        let view = render(&mut chat, 32, 16)?;
        assert!(
            view.lines().nth(1).is_some_and(|line| line == "  line-19"),
            "{view}"
        );
        chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (2, 1)));
        assert_eq!(
            chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (8, 1))),
            Action::Copy("line-19".into())
        );
        if !stopped {
            chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (2, 1)));
            chat.observe(&Observation::Event(EngineEvent {
                invocation: cocoa_core::store::InvocationId(1),
                kind: EventKind::AssistantText {
                    text: "\nnew text".into(),
                    truncated: false,
                },
            }));
            assert_eq!(
                chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (8, 1))),
                Action::None
            );
        }
    }
    Ok(())
}

#[test]
fn mouse_wheel_scrolls_output_in_each_phase_without_editing_the_draft() -> Result<(), Box<dyn Error>>
{
    use cocoa_core::engine::{Event as EngineEvent, EventKind, Observation};
    let answer = (0..20)
        .map(|index| format!("line-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    for phase in ["idle", "running", "stopped"] {
        let mut chat = Chat::new(64)?;
        chat.input(key(KeyCode::Char('a'), KeyModifiers::NONE));
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        match phase {
            "idle" => chat.complete(&answer),
            "running" => chat.observe(&Observation::Event(EngineEvent {
                invocation: cocoa_core::store::InvocationId(1),
                kind: EventKind::AssistantText {
                    text: answer.clone(),
                    truncated: false,
                },
            })),
            _ => chat.fail(&answer),
        }
        if phase == "idle" {
            for character in "untouched draft".chars() {
                chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
            }
        }
        let bottom = render(&mut chat, 64, 16)?;
        assert!(bottom.contains("line-19"), "{bottom}");
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 12,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        };
        assert_eq!(chat.input(wheel(MouseEventKind::ScrollUp)), Action::Redraw);
        let scrolled = render(&mut chat, 64, 16)?;
        assert!(scrolled.contains("line-08"), "{scrolled}");
        assert!(!scrolled.contains("line-19"), "{scrolled}");
        chat.input(wheel(MouseEventKind::ScrollDown));
        assert_eq!(render(&mut chat, 64, 16)?, bottom);
        chat.input(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_ne!(render(&mut chat, 64, 16)?, bottom);
        chat.input(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(render(&mut chat, 64, 16)?, bottom);
        for _ in 0..100 {
            chat.input(wheel(MouseEventKind::ScrollUp));
            render(&mut chat, 64, 16)?;
        }
        let top = render(&mut chat, 64, 16)?;
        chat.input(wheel(MouseEventKind::ScrollUp));
        assert_eq!(render(&mut chat, 64, 16)?, top);
        for _ in 0..100 {
            chat.input(wheel(MouseEventKind::ScrollDown));
        }
        assert_eq!(render(&mut chat, 64, 16)?, bottom);
        assert_eq!(chat.input(wheel(MouseEventKind::ScrollLeft)), Action::None);
        if phase == "idle" {
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit("untouched draft".into())
            );
        }
    }
    Ok(())
}

#[test]
fn exact_exit_commands_close_without_submitting() -> Result<(), Box<dyn Error>> {
    for command in ["exit", ":q", ":x"] {
        for phase in ["idle", "running", "stopping"] {
            let mut chat = Chat::new(64)?;
            if phase != "idle" {
                type_text(&mut chat, "run");
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            }
            type_text(&mut chat, command);
            if phase == "stopping" {
                chat.stopping();
            }
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Exit,
                "command={command:?}, phase={phase}"
            );
        }
    }
    for prompt in [
        "exit now",
        "please exit",
        " exit",
        "exit ",
        "exit\n",
        "EXIT",
        ":q!",
        ":quit",
        ":x file",
        " :q",
        ":q\n:x",
        "\n:x",
    ] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, prompt);
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(prompt.into()),
            "prompt={prompt:?}"
        );
    }
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, ":q");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::CONTROL)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(":q\n".into())
    );
    Ok(())
}

#[test]
fn enter_sends_and_control_enter_inserts_a_newline() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(8)?;
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    chat.input(key(KeyCode::Char('é'), KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::CONTROL)),
        Action::Redraw
    );
    chat.input(key(KeyCode::Char('🦀'), KeyModifiers::NONE));
    for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
        assert_eq!(
            chat.input(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind,
            ))),
            Action::None
        );
    }
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("é\n🦀".into())
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn steering_keeps_the_invocation_active_and_recovers_rejected_drafts() -> Result<(), Box<dyn Error>>
{
    let mut chat = Chat::new(4096)?;
    chat.initial_prompt("initial task")?;
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(7),
        kind: EventKind::AssistantText {
            text: "old provisional answer".into(),
            truncated: false,
        },
    }));
    chat.paste_text("instead use λ\nwith tests")?;
    let expected = UserInput::from("instead use λ\nwith tests");
    assert!(chat.can_submit());
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Steer(expected.clone())
    );
    chat.reject_steering("Steering queue is full");
    let view = render(&mut chat, 70, 20)?;
    assert!(view.contains("Steering queue is full"), "{view}");
    assert!(view.contains("│  instead use λ"), "{view}");
    assert!(view.contains("│  with tests"), "{view}");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Steer(expected.clone())
    );
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(7),
        kind: EventKind::SteeringApplied,
    }));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(7),
        kind: EventKind::AssistantText {
            text: "new provisional answer".into(),
            truncated: false,
        },
    }));
    let view = render(&mut chat, 70, 20)?;
    assert!(!view.contains("old provisional answer"), "{view}");
    assert!(view.contains("new provisional answer"), "{view}");
    chat.paste_text("/model high")?;
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model high".into())
    );
    chat.input(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    chat.input(key(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Steer(expected)
    );
    chat.paste_text("next draft")?;
    chat.stopping();
    assert!(!chat.can_submit());
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    chat.cancelled();
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("next draft".into())
    );
    Ok(())
}

#[test]
fn running_draft_stays_editable_and_only_submits_after_completion() -> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{Event as EngineEvent, EventKind, Observation};
    let mut chat = Chat::new(8)?;
    chat.set_message_steering(false);
    chat.input(key(KeyCode::Char('a'), KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("a".into())
    );
    for (code, modifiers) in [
        (KeyCode::Char('é'), KeyModifiers::NONE),
        (KeyCode::Enter, KeyModifiers::CONTROL),
        (KeyCode::Char('🦀'), KeyModifiers::NONE),
        (KeyCode::Char('!'), KeyModifiers::NONE),
    ] {
        assert_eq!(chat.input(key(code, modifiers)), Action::Redraw);
    }
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    chat.input(key(KeyCode::Backspace, KeyModifiers::NONE));
    chat.input(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
    chat.input(key(KeyCode::Char('λ'), KeyModifiers::NONE));
    chat.input(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    chat.input(key(KeyCode::Char('?'), KeyModifiers::NONE));
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: cocoa_core::store::InvocationId(1),
        kind: EventKind::AssistantText {
            text: "streaming reply".into(),
            truncated: false,
        },
    }));
    let view = render(&mut chat, 64, 16)?;
    assert!(view.contains("Toasting"), "{view}");
    assert!(view.contains("streaming reply"), "{view}");
    assert!(view.contains("│  é"), "{view}");
    assert!(view.contains("│  λ"), "{view}");
    assert!(view.contains("│  ?"), "{view}");
    chat.complete("finished reply");
    let view = render(&mut chat, 64, 16)?;
    assert!(view.contains("finished reply"), "{view}");
    assert!(view.contains("│  λ"), "{view}");
    assert!(!view.contains("Toasting"), "{view}");
    chat.input(key(KeyCode::Char('!'), KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("é\nλ\n!?".into())
    );
    chat.input(key(KeyCode::Char('z'), KeyModifiers::NONE));
    chat.fail("stopped");
    assert!(render(&mut chat, 64, 16)?.contains("│  z"));
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn slash_commands_complete_and_run_during_a_turn_without_steering() -> Result<(), Box<dyn Error>> {
    use cocoa::theme::Theme;

    let mut chat = Chat::new(4096)?;
    chat.set_message_steering(false);
    chat.initial_prompt("initial task")?;
    assert!(!chat.can_submit());
    type_text(&mut chat, "/theme");
    assert!(render(&mut chat, 80, 24)?.contains("/theme milk-chocolate"));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(chat.theme(), Theme::MilkChocolate);

    type_text(&mut chat, "/us");
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/usage".into())
    );
    type_text(&mut chat, "/model");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Command("/model".into())
    );
    type_text(&mut chat, "not a command");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );

    chat.input(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    type_text(&mut chat, "/usage");
    chat.stopping();
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn composer_wraps_without_indenting_separator_spaces() -> Result<(), Box<dyn Error>> {
    for width in [18, 24, 32] {
        let word = "a".repeat(usize::from(width - 6));
        let prompt = format!("{word} target tail");
        let mut chat = Chat::new(1024)?;
        type_text(&mut chat, &prompt);
        let view = render(&mut chat, width, 20)?;
        let row = view
            .lines()
            .position(|line| line.starts_with("│  target"))
            .ok_or_else(|| format!("wrapped word is indented: {view}"))? as u16;
        assert!(!view.contains("│   target"), "{view}");
        for _ in 0..11 {
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        }
        for _ in 0..2 {
            let mut terminal = Terminal::new(TestBackend::new(width, 20))?;
            terminal.draw(|frame| chat.render(frame))?;
            assert!(
                terminal.backend().buffer()[(3, row)]
                    .modifier
                    .contains(ratatui::style::Modifier::REVERSED)
            );
            chat.input(key(KeyCode::Left, KeyModifiers::NONE));
        }
        render(&mut chat, width, 20)?;
        chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (3, row)));
        assert_eq!(
            chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (8, row))),
            Action::Copy("target".into())
        );
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(prompt.into())
        );
        chat.complete("done");
        type_text(&mut chat, "first\n target");
        assert!(render(&mut chat, width, 20)?.contains("│   target"));
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("first\n target".into())
        );
    }
    Ok(())
}

#[test]
fn composer_keeps_space_separated_text_visible_when_it_fits() -> Result<(), Box<dyn Error>> {
    for (prompt, words) in [
        (
            "one two three four five six seven eight nine ten",
            vec![
                "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
            ],
        ),
        (
            "one      two      three      four      five      six",
            vec!["one", "two", "three", "four", "five", "six"],
        ),
        ("first                    last", vec!["first", "last"]),
        ("   café      λ      fin   ", vec!["café", "λ", "fin"]),
        ("first\n                    last", vec!["first", "last"]),
    ] {
        let mut chat = Chat::new(1024)?;
        render(&mut chat, 24, 20)?;
        type_text(&mut chat, prompt);
        for width in [24, 64, 24] {
            let view = render(&mut chat, width, 20)?;
            for word in &words {
                assert!(view.contains(word), "missing {word:?}:\n{view}");
            }
        }
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(prompt.into())
        );
    }
    Ok(())
}

#[test]
fn empty_view_shows_cocoa_above_a_multiline_composer() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(16384)?;
    chat.set_context("~/Development/cocoa", "medium");
    let mut terminal = Terminal::new(TestBackend::new(64, 12))?;
    terminal.draw(|frame| chat.render(frame))?;
    let buffer = terminal.backend().buffer();
    let text = buffer
        .content
        .chunks(64)
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let brand_row = text
        .lines()
        .position(|row| row.trim() == "cocoa")
        .ok_or("missing brand")?;
    let composer_row = text
        .lines()
        .position(|row| row.starts_with('╭'))
        .ok_or("missing composer")?;
    assert!(brand_row < composer_row, "{text}");
    assert!(text.contains("medium"), "{text}");
    assert!(text.contains("~/Development/cocoa"), "{text}");
    let composer = text
        .lines()
        .filter(|row| row.starts_with('│'))
        .collect::<Vec<_>>();
    assert!(composer.len() >= 2, "{text}");
    assert!(
        composer
            .iter()
            .all(|row| row.trim_matches(['│', ' ']).is_empty()),
        "{text}"
    );
    Ok(())
}

#[test]
fn bracketed_paste_inserts_line_breaks_and_never_submits() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(1024)?;
    assert_eq!(
        chat.input(Event::Paste("first line\r\nsecond line".into())),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("first line\nsecond line".into())
    );
    chat.fail("setup failed");
    assert_eq!(
        chat.input(Event::Paste("ignored in the stopped view".into())),
        Action::None
    );
    Ok(())
}

#[test]
fn modified_enter_and_unicode_edits_fit_the_byte_budget() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(8)?;
    for code in [
        KeyCode::Char('é'),
        KeyCode::Enter,
        KeyCode::Char('🦀'),
        KeyCode::Char('!'),
    ] {
        let modifiers = if code == KeyCode::Enter {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        assert_eq!(chat.input(key(code, modifiers)), Action::Redraw);
    }
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(Event::Paste("not accepted".into())),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('v'), KeyModifiers::CONTROL)),
        Action::Paste
    );
    assert_eq!(
        chat.input(key(KeyCode::Backspace, KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('?'), KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("é\n🦀?".into())
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Action::Redraw
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Action::Exit
    );
    let mut chat = Chat::new(16384)?;
    for _ in 0..100 {
        for _ in 0..60 {
            chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE));
        }
        render(&mut chat, 40, 12)?;
    }
    chat.input(key(KeyCode::Enter, KeyModifiers::CONTROL));
    for character in "second line".chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let view = render(&mut chat, 40, 12)?;
    assert!(view.contains("second line"), "{view}");
    let mut chat = Chat::new(32)?;
    for code in [
        KeyCode::Char('a'),
        KeyCode::Char('c'),
        KeyCode::Enter,
        KeyCode::Char('x'),
        KeyCode::Char('z'),
        KeyCode::Up,
        KeyCode::Left,
        KeyCode::Char('b'),
        KeyCode::Down,
        KeyCode::Home,
        KeyCode::Delete,
        KeyCode::End,
        KeyCode::Char('!'),
    ] {
        let modifiers = if code == KeyCode::Enter {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        chat.input(key(code, modifiers));
    }
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("abc\nz!".into())
    );
    Ok(())
}

#[test]
fn completion_replaces_provisional_text_and_failure_stops_submission() -> Result<(), Box<dyn Error>>
{
    use cocoa_core::engine::{Event as EngineEvent, EventKind, Observation};
    let mut chat = Chat::new(32)?;
    chat.set_context("~/Development/cocoa", "medium");
    for character in "hello".chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("hello".into())
    );
    chat.observe(&Observation::Event(EngineEvent {
        invocation: cocoa_core::store::InvocationId(1),
        kind: EventKind::AssistantText {
            text: "guessed answer".into(),
            truncated: false,
        },
    }));
    let view = render(&mut chat, 64, 12)?;
    assert!(view.contains("guessed answer"), "{view}");
    assert!(view.contains("Toasting"), "{view}");
    chat.complete("final answer");
    let view = render(&mut chat, 64, 12)?;
    assert!(view.contains("hello"), "{view}");
    assert!(view.contains("final answer"), "{view}");
    assert!(!view.contains("guessed answer"), "{view}");
    assert!(!view.contains("Toasting"), "{view}");
    for character in "again".chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(_)
    ));
    chat.observe(&Observation::Lagged { missed: 23 });
    chat.fail("model provider failed");
    let view = render(&mut chat, 64, 16)?;
    assert!(view.contains("final answer"), "{view}");
    assert!(view.contains("again"), "{view}");
    assert!(view.contains("Error: model provider failed"), "{view}");
    assert!(view.contains("Stopped"), "{view}");
    assert!(view.contains("Ctrl+C to close"), "{view}");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    chat.complete("late result must not restart a stopped engine");
    assert_eq!(
        chat.input(key(KeyCode::Char('x'), KeyModifiers::NONE)),
        Action::None
    );
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::None
    );
    Ok(())
}

#[test]
fn wrapped_composer_keeps_the_theme_background() -> Result<(), Box<dyn Error>> {
    use cocoa::theme::Theme;

    for theme in [
        Theme::DarkChocolate,
        Theme::CacaoGrove,
        Theme::MilkChocolate,
    ] {
        let mut chat = Chat::new(1024)?;
        chat.set_theme(theme);
        type_text(&mut chat, "aaaaa bbbb cccc ddddd eeee fffff gggg");
        for width in 12..40 {
            let mut terminal = Terminal::new(TestBackend::new(width, 14))?;
            terminal.draw(|frame| chat.render(frame))?;
            let buffer = terminal.backend().buffer();
            assert!(
                (0..14).any(|row| buffer[(0, row)].symbol() == "│"),
                "missing composer"
            );
            for row in 0..14 {
                if buffer[(0, row)].symbol() != "│" {
                    continue;
                }
                for column in 1..width - 1 {
                    assert_eq!(
                        buffer[(column, row)].bg,
                        theme.palette().background,
                        "{theme:?}, width {width}, cell ({column}, {row})"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn multiline_unicode_and_narrow_views_preserve_the_simple_layout() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(32)?;
    chat.set_context("~/Development/cocoa", "medium");
    for code in [KeyCode::Char('é'), KeyCode::Enter, KeyCode::Char('🦀')] {
        let modifiers = if code == KeyCode::Enter {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::NONE
        };
        chat.input(key(code, modifiers));
    }
    for (width, height) in [(64, 12), (16, 8)] {
        let view = render(&mut chat, width, height)?;
        assert!(view.contains("cocoa"), "{view}");
        assert!(view.contains("medium"), "{view}");
        let rows = view
            .lines()
            .filter(|line| line.starts_with('│'))
            .collect::<Vec<_>>();
        let accent = rows
            .iter()
            .position(|row| row.contains('é'))
            .ok_or("missing accent")?;
        let crab = rows
            .iter()
            .position(|row| row.contains('🦀'))
            .ok_or("missing crab")?;
        assert_eq!(crab, accent + 1, "{view}");
        assert!(rows.iter().all(|row| row.ends_with('│')), "{view}");
    }
    for height in [9, 10] {
        let view = render(&mut chat, 40, height)?;
        let rows = view
            .lines()
            .filter(|line| line.starts_with('│'))
            .collect::<Vec<_>>();
        if height == 10 {
            assert_eq!(rows.len(), 4, "{view}");
            assert!(rows[0].trim_matches(['│', ' ']).is_empty(), "{view}");
            assert!(rows[3].trim_matches(['│', ' ']).is_empty(), "{view}");
            assert!(rows[1].starts_with("│  é"), "{view}");
        } else {
            assert_eq!(rows.len(), 2, "{view}");
            assert!(rows[0].starts_with("│  é"), "{view}");
        }
    }
    for (width, height) in [(0, 0), (1, 1), (2, 2)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| chat.render(frame))?;
    }
    let mut chat = Chat::new(128)?;
    let draft = format!("A{}Z", "x".repeat(118));
    for character in draft.chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    let view = render(&mut chat, 64, 14)?;
    let displayed = view
        .lines()
        .filter(|line| line.starts_with('│'))
        .map(|line| line.trim_matches(['│', ' ']))
        .collect::<String>();
    assert_eq!(displayed, draft, "{view}");
    Ok(())
}

#[test]
fn long_transcript_lines_render_and_copy_beyond_u16_rows() -> Result<(), Box<dyn Error>> {
    for (width, repeated) in [(9, "x"), (9, "x\n"), (9, "界x"), (11, "é界")] {
        let mut chat = Chat::new(512 * 1024)?;
        let prompt = format!("A{}Z", repeated.repeat(70_000));
        assert_eq!(chat.initial_prompt(&prompt)?, Action::Submit(prompt.into()));
        chat.cancelled();
        let bottom = render(&mut chat, width, 16)?;
        let (row, column) = bottom
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                line.chars()
                    .position(|character| character == 'Z')
                    .map(|column| (row as u16, column as u16))
            })
            .ok_or_else(|| format!("missing transcript tail: {bottom}"))?;
        chat.input(mouse(
            MouseEventKind::Down(MouseButton::Left),
            (column, row),
        ));
        chat.input(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            (column + 1, row),
        ));
        assert_eq!(
            chat.input(mouse(
                MouseEventKind::Up(MouseButton::Left),
                (column + 1, row)
            )),
            Action::Copy("Z".into())
        );
        for _ in 0..14_100 {
            chat.input(key(KeyCode::PageUp, KeyModifiers::NONE));
        }
        let top = render(&mut chat, width, 16)?;
        assert!(top.contains('A'), "{top}");
        for _ in 0..14_100 {
            chat.input(key(KeyCode::PageDown, KeyModifiers::NONE));
        }
        assert_eq!(render(&mut chat, width, 16)?, bottom);
        let resized = render(&mut chat, width + 17, 16)?;
        assert!(resized.contains('Z'), "{resized}");
        assert_eq!(render(&mut chat, width, 16)?, bottom);
    }
    Ok(())
}

#[test]
fn transcript_keeps_all_entries_and_scrolls_past_the_former_byte_limit()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    for index in 0..300 {
        chat.notice(&format!(
            "entry-{index:03}\n{}end-{index:03}",
            "line\n".repeat(120)
        ));
    }
    let bottom = render(&mut chat, 64, 16)?;
    assert!(bottom.contains("end-299"), "{bottom}");
    assert!(!bottom.contains("omitted"), "{bottom}");
    for _ in 0..8000 {
        chat.input(key(KeyCode::PageUp, KeyModifiers::NONE));
    }
    let top = render(&mut chat, 64, 16)?;
    assert!(top.contains("entry-000"), "{top}");
    assert!(!top.contains("omitted"), "{top}");
    for _ in 0..8000 {
        chat.input(key(KeyCode::PageDown, KeyModifiers::NONE));
    }
    assert_eq!(render(&mut chat, 64, 16)?, bottom);
    for _ in 0..13000 {
        chat.input(mouse(MouseEventKind::ScrollUp, (12, 3)));
    }
    assert_eq!(render(&mut chat, 64, 16)?, top);
    Ok(())
}

#[test]
fn provisional_text_disappears_when_tools_start() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(64)?;
    type_text(&mut chat, "run");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    for text in ["é".repeat(9000), "\npreview-tail".into()] {
        chat.observe(&Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind: EventKind::AssistantText {
                text,
                truncated: false,
            },
        }));
    }
    let streaming = render(&mut chat, 64, 16)?;
    assert!(streaming.contains("preview-tail"), "{streaming}");
    assert!(streaming.contains("é"), "{streaming}");
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::ModelFinished { executing: true },
    }));
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    render(&mut chat, 64, 16)?;
    for _ in 0..1000 {
        chat.input(key(KeyCode::PageDown, KeyModifiers::NONE));
    }
    let commentary = render(&mut chat, 64, 16)?;
    assert!(!commentary.contains("preview-tail"), "{commentary}");
    assert!(!commentary.contains("Assistant commentary"), "{commentary}");
    assert!(!commentary.contains("omitted"), "{commentary}");
    Ok(())
}

#[test]
fn complete_history_and_terminal_controls_cannot_turn_previews_into_answers()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::engine::{Event as EngineEvent, EventKind, Observation};
    let mut chat = Chat::new(32)?;
    chat.input(key(KeyCode::Char('a'), KeyModifiers::NONE));
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: cocoa_core::store::InvocationId(1),
        kind: EventKind::AssistantText {
            text: "safe\x1b\0\r".into(),
            truncated: false,
        },
    }));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: cocoa_core::store::InvocationId(1),
        kind: EventKind::AnswerCompleted {
            text: "not-authoritative".into(),
            truncated: false,
        },
    }));
    let view = render(&mut chat, 64, 12)?;
    assert!(view.contains("safe"));
    assert!(!view.contains("Preview:"));
    assert!(!view.contains("not-authoritative"));
    assert!(
        !view
            .chars()
            .any(|character| character.is_control() && character != '\n')
    );
    chat.complete("old-marker");
    chat.input(key(KeyCode::Char('b'), KeyModifiers::NONE));
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: cocoa_core::store::InvocationId(2),
        kind: EventKind::AssistantText {
            text: "preview".repeat(3000),
            truncated: false,
        },
    }));
    assert!(!render(&mut chat, 64, 12)?.contains("Live output incomplete"));
    chat.complete(&("é".repeat(20000) + "\nanswer-tail"));
    let completed = render(&mut chat, 64, 12)?;
    assert!(completed.contains("answer-tail"), "{completed}");
    assert!(!completed.contains("omitted"), "{completed}");
    for _ in 0..7000 {
        chat.input(key(KeyCode::PageUp, KeyModifiers::NONE));
    }
    let view = render(&mut chat, 64, 12)?;
    assert!(view.contains("old-marker"), "{view}");
    assert!(!view.contains("omitted"), "{view}");
    assert!(!view.contains("preview"));
    chat.input(key(KeyCode::Char('c'), KeyModifiers::NONE));
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("newest answer");
    assert!(render(&mut chat, 64, 12)?.contains("newest answer"));
    assert!(Chat::new(0).is_err());
    assert!(Chat::new(8 * 1024 * 1024 + 1).is_err());
    Ok(())
}

#[test]
fn user_background_context_and_activity_survive_wrapping_and_completion()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    chat.set_context("~/Development/cocoa", "x-high");
    assert!(!chat.tick(Duration::from_millis(40)));
    for character in "A user message long enough to wrap in this viewport".chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    chat.input(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    chat.input(key(KeyCode::Char('λ'), KeyModifiers::NONE));
    assert!(matches!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit(_)
    ));
    let first = render(&mut chat, 40, 16)?;
    assert!(first.contains("x-high"));
    assert!(first.contains("~/Development/cocoa"));
    assert!(!first.contains("Output"));
    assert!(!first.contains("You:"));
    let narrow = render(&mut chat, 16, 16)?;
    assert!(narrow.contains("Toasting"), "{narrow}");
    assert!(!narrow.contains("x-high"), "{narrow}");
    for _ in 0..3 {
        chat.tick(Duration::from_millis(40));
    }
    assert_eq!(render(&mut chat, 40, 16)?, first);
    chat.tick(Duration::from_millis(360));
    assert_ne!(render(&mut chat, 40, 16)?, first);
    chat.complete("Assistant response");
    assert!(!chat.tick(Duration::from_millis(40)));
    let mut terminal = Terminal::new(TestBackend::new(40, 16))?;
    terminal.draw(|frame| chat.render(frame))?;
    let buffer = terminal.backend().buffer();
    let background = buffer[(2, 0)].bg;
    for row in 0..5 {
        for column in 2..38 {
            assert_eq!(buffer[(column, row)].bg, background);
        }
        for column in [0, 1, 38, 39] {
            assert_ne!(buffer[(column, row)].bg, background);
        }
    }
    assert_ne!(background, buffer[(2, 6)].bg);
    let view = render(&mut chat, 40, 16)?;
    assert!(view.contains("Assistant response"));
    assert!(!view.contains("Toasting"));
    assert!(!view.contains("Answer:"));
    let mut chat = Chat::new(128)?;
    for character in "0123456789".repeat(6).chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    chat.input(key(KeyCode::Enter, KeyModifiers::CONTROL));
    for character in "last-line".chars() {
        chat.input(key(KeyCode::Char(character), KeyModifiers::NONE));
    }
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.complete("reply");
    let view = render(&mut chat, 24, 12)?;
    assert_eq!(view.lines().next(), Some("    890123456789"));
    assert!(view.contains("    last-line"), "{view}");
    assert!(view.contains("  reply"), "{view}");
    chat.input(key(KeyCode::PageUp, KeyModifiers::NONE));
    let view = render(&mut chat, 24, 12)?;
    assert_eq!(view.lines().nth(1), Some("    0123456789012345"));
    assert!(!view.contains("reply"), "{view}");
    Ok(())
}

#[test]
fn pod_press_keeps_its_timing_and_fixed_group_slot_and_stops_on_completion()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(32)?;
    let mut model_only = Chat::new(32)?;
    model_only.initial_prompt("question")?;
    chat.input(key(KeyCode::Char('a'), KeyModifiers::NONE));
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind: EventKind::CapabilityStarted {
            operation: OperationId(1),
            name: "cocoa.read".into(),
            truncated: false,
        },
    }));
    chat.observe_agent(&AgentUpdate {
        session: 10,
        agent: fixture_agent("Peer"),
        name: "Peer".into(),
        kind: AgentUpdateKind::Started,
    });
    let first = render(&mut chat, 40, 16)?;
    let position = first
        .lines()
        .find_map(|line| line.find("Toasting"))
        .ok_or("missing work header")?;
    assert_eq!(position, 6, "{first}");
    for (elapsed, full, mini, label, changed) in [
        (Duration::ZERO, " (o|o) ", "(o)", "Toasting", false),
        (
            Duration::from_millis(120),
            " (o|o) ",
            "(o)",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(120),
            " (o|o) ",
            "(o)",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(120),
            " (o|o) ",
            "(o)",
            "Toasting",
            false,
        ),
        (
            Duration::from_nanos(119_999_999),
            " (o|o) ",
            "(o)",
            "Toasting",
            false,
        ),
        (Duration::from_nanos(1), " (|||) ", "(|)", "Toasting", true),
        (
            Duration::from_millis(120),
            " [|||] ",
            "[|]",
            "Toasting",
            true,
        ),
        (
            Duration::from_millis(120),
            " [:::] ",
            "[:]",
            "Toasting",
            true,
        ),
        (
            Duration::from_millis(120),
            " [:::] ",
            "[:]",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(120),
            " [###] ",
            "[#]",
            "Toasting",
            true,
        ),
        (
            Duration::from_millis(120),
            " [###] ",
            "[#]",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(120),
            " [###] ",
            "[#]",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(120),
            " [###] ",
            "[#]",
            "Toasting",
            false,
        ),
        (
            Duration::from_nanos(119_999_999),
            " [###] ",
            "[#]",
            "Toasting",
            false,
        ),
        (Duration::from_nanos(1), " [:::] ", "[:]", "Toasting", true),
        (
            Duration::from_millis(120),
            " [:::] ",
            "[:]",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(120),
            " [|||] ",
            "[|]",
            "Toasting",
            true,
        ),
        (
            Duration::from_millis(120),
            " (|||) ",
            "(|)",
            "Toasting",
            true,
        ),
        (
            Duration::from_millis(120),
            " (o|o) ",
            "(o)",
            "Toasting",
            true,
        ),
        (
            Duration::from_nanos(3_839_999_999),
            " (|||) ",
            "(|)",
            "Toasting",
            true,
        ),
        (Duration::from_nanos(1), " (o|o) ", "(o)", "Blending", true),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Refining",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Winnowing",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Conching",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Setting",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Finishing",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Tempering",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Stirring",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Cracking",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Melting",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Milling",
            true,
        ),
        (
            Duration::from_millis(5760),
            " (o|o) ",
            "(o)",
            "Toasting",
            true,
        ),
        (
            Duration::from_millis(69_120),
            " (o|o) ",
            "(o)",
            "Toasting",
            false,
        ),
        (
            Duration::from_millis(67_920),
            " [:::] ",
            "[:]",
            "Milling",
            true,
        ),
        (
            Duration::from_secs(86_400) + Duration::from_millis(600),
            " [###] ",
            "[#]",
            "Milling",
            true,
        ),
        (
            Duration::from_millis(5760),
            " [###] ",
            "[#]",
            "Toasting",
            true,
        ),
    ] {
        assert_eq!(
            chat.tick(elapsed),
            changed,
            "{elapsed:?}: {full} {mini} {label}"
        );
        assert_eq!(
            model_only.tick(elapsed),
            changed,
            "model only {elapsed:?}: {full} {mini} {label}"
        );
        assert_eq!(full.len(), 7);
        assert_eq!(mini.len(), 3);
        for width in [32, 40, 80] {
            let view = render(&mut chat, width, 16)?;
            assert!(view.contains(&format!("  {mini} {label} ▾")), "{view}");
            assert_eq!(view.matches(full).count(), 1, "{view}");
            assert_eq!(view.matches(mini).count(), 2, "{view}");
            assert!(
                view.contains(&format!("{mini} Peer Agent {label} ▸")),
                "{view}"
            );
            assert_eq!(view.matches(label).count(), 3, "{view}");
            assert_eq!(
                view.lines().find_map(|line| line.find(label)),
                Some(position),
                "{view}"
            );
            assert!(view.contains(&format!("╭ {full}{label} ─")), "{view}");
            assert!(view.contains("Reading file"), "{view}");
            let view = render(&mut model_only, width, 16)?;
            assert!(
                view.lines().any(|line| line == format!("  {mini} {label}")),
                "{view}"
            );
            assert!(view.contains(&format!("╭ {full}{label} ─")), "{view}");
            assert_eq!(view.matches(full).count(), 1, "{view}");
            assert_eq!(view.matches(mini).count(), 1, "{view}");
            assert_eq!(view.matches(label).count(), 2, "{view}");
        }
        for width in [1, 8, 16, 18] {
            let narrow = render(&mut chat, width, 16)?;
            for glyph in [
                "(o|o)", "(|||)", "[|||]", "[:::]", "[###]", "(o)", "(|)", "[|]", "[:]", "[#]",
            ] {
                assert!(!narrow.contains(glyph), "{width}: {narrow}");
            }
        }
        for width in [19, 22] {
            let narrow = render(&mut chat, width, 16)?;
            assert!(
                narrow.contains(&format!("  {mini} {label} ▾")),
                "{width}: {narrow}"
            );
        }
    }
    chat.set_context(".", "medium");
    for (width, full, alias) in [
        (19, false, false),
        (20, true, false),
        (26, true, false),
        (27, true, true),
        (28, true, true),
    ] {
        let view = render(&mut chat, width, 16)?;
        let title = view
            .lines()
            .find(|line| line.starts_with('╭'))
            .ok_or("missing composer")?;
        assert_eq!(title.contains(" [###] "), full, "{view}");
        assert_eq!(title.contains("medium"), alias, "{view}");
        assert!(title.contains("Toasting"), "{view}");
        if !full {
            assert!(title.starts_with("╭ Toasting "), "{view}");
        }
    }
    chat.observe_agent(&AgentUpdate {
        session: 10,
        agent: fixture_agent("Peer"),
        name: "Peer".into(),
        kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
    });
    let settled_agent = render(&mut chat, 40, 16)?;
    assert!(
        settled_agent.contains("Peer Agent Worked ▸"),
        "{settled_agent}"
    );
    chat.complete("finished");
    assert!(!chat.tick(Duration::from_secs(10)));
    let completed = render(&mut chat, 40, 16)?;
    for glyph in ["[###]", "[#]"] {
        assert!(!completed.contains(glyph), "{completed}");
    }
    assert_eq!(
        completed.lines().find_map(|line| line.find("Work Done")),
        Some(2),
        "{completed}"
    );
    assert_eq!(completed, render(&mut chat, 40, 16)?);
    for width in [18, 19, 40] {
        let view = render(&mut chat, width, 16)?;
        let header = view
            .lines()
            .find(|line| line.contains("Work Done"))
            .ok_or("missing completed work")?;
        assert_eq!(header.find("Work Done"), Some(2), "{view}");
        let title = view
            .lines()
            .find(|line| line.starts_with('╭'))
            .ok_or("missing idle composer")?;
        assert!(!title.contains("Toasting"), "{view}");
        assert!(!title.contains("[###]"), "{view}");
    }
    model_only.complete("finished");
    let completed = render(&mut model_only, 40, 16)?;
    assert!(!chat.tick(Duration::MAX));
    assert!(!model_only.tick(Duration::MAX));
    assert_eq!(render(&mut model_only, 40, 16)?, completed);
    for text in ["Toasting", "[###]", "[#]"] {
        assert!(!completed.contains(text), "{completed}");
    }
    Ok(())
}

#[test]
fn direct_model_selection_shows_its_reasoning_effort_until_the_title_narrows()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    chat.set_context(".", "openai/gpt-5");
    chat.set_thinking(Some("high"));
    for width in [16, 24, 40, 80] {
        let view = render(&mut chat, width, 16)?;
        assert!(view.contains("gpt-5 (high)"), "{width}: {view}");
        assert!(!view.contains("openai/"), "{width}: {view}");
    }
    for width in [9, 12, 15] {
        let view = render(&mut chat, width, 16)?;
        assert!(view.contains("gpt-5"), "{width}: {view}");
        assert!(!view.contains("openai/"), "{width}: {view}");
        assert!(!view.contains("(high)"), "{width}: {view}");
    }
    let narrow = render(&mut chat, 8, 16)?;
    assert!(!narrow.contains("gpt-5"), "{narrow}");
    chat.set_thinking(None);
    let view = render(&mut chat, 80, 16)?;
    assert!(view.contains("gpt-5"), "{view}");
    assert!(!view.contains("openai/"), "{view}");
    assert!(!view.contains('('), "{view}");
    Ok(())
}

#[test]
fn pod_press_is_static_in_reduced_motion_and_quiet_during_recovery() -> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    for reduced_motion in [false, true] {
        let mut chat = Chat::new(128)?;
        chat.set_reduced_motion(reduced_motion);
        chat.initial_prompt("check")?;
        let full = if reduced_motion { " [###] " } else { " (o|o) " };
        let mini = if reduced_motion { "[#]" } else { "(o)" };
        let initial = render(&mut chat, 40, 16)?;
        assert!(initial.contains(&format!("  {mini} Toasting")), "{initial}");
        assert!(
            initial.contains(&format!("╭ {full}Toasting ─")),
            "{initial}"
        );
        assert_eq!(initial.matches(full).count(), 1, "{initial}");
        assert_eq!(initial.matches(mini).count(), 1, "{initial}");
        assert_eq!(chat.tick(Duration::from_millis(480)), !reduced_motion);
        let full = if reduced_motion { " [###] " } else { " (|||) " };
        let mini = if reduced_motion { "[#]" } else { "(|)" };
        type_text(&mut chat, "keep draft");
        observe_root_event(
            &mut chat,
            EventKind::CapabilityStarted {
                operation: OperationId(1),
                name: "cocoa.read".into(),
                truncated: false,
            },
        );
        chat.observe_agent(&AgentUpdate {
            session: 10,
            agent: fixture_agent("Peer"),
            name: "Peer".into(),
            kind: AgentUpdateKind::Started,
        });
        let active = render(&mut chat, 40, 20)?;
        assert!(active.contains(&format!("  {mini} Toasting ▾")), "{active}");
        assert!(active.contains(&format!("╭ {full}Toasting ─")), "{active}");
        assert_eq!(active.matches("Toasting").count(), 3, "{active}");
        assert_eq!(active.matches(full).count(), 1, "{active}");
        assert_eq!(active.matches(mini).count(), 2, "{active}");
        assert!(
            active.contains(&format!("{mini} Peer Agent Toasting ▸")),
            "{active}"
        );
        if reduced_motion {
            for elapsed in [
                Duration::from_nanos(5_759_999_999),
                Duration::from_nanos(1),
                Duration::from_nanos(5_759_999_999),
                Duration::from_nanos(1),
                Duration::from_nanos(5_759_999_999),
                Duration::from_nanos(1),
                Duration::from_millis(69_120),
                Duration::from_secs(86_400) + Duration::from_millis(600),
                Duration::MAX,
            ] {
                assert!(!chat.tick(elapsed));
                assert_eq!(render(&mut chat, 40, 20)?, active);
            }
        }
        assert!(!chat.tick(Duration::from_nanos(5_279_999_999)));
        let waiting = render(&mut chat, 40, 20)?;
        assert!(
            waiting.contains(&format!("  {mini} Toasting ▾")),
            "{waiting}"
        );
        assert!(
            waiting.contains(&format!("╭ {full}Toasting ─")),
            "{waiting}"
        );
        observe_root_event(
            &mut chat,
            EventKind::ProviderRetryScheduled {
                status: 503,
                retry_at: now + Duration::from_secs(60),
            },
        );
        assert!(!chat.tick_at(Duration::from_millis(100), now + Duration::from_millis(100)));
        assert!(chat.tick_at(Duration::from_millis(900), now + Duration::from_secs(1)));
        let view = render(&mut chat, 40, 16)?;
        assert!(!view.contains(full), "{view}");
        assert!(!view.contains(mini), "{view}");
        assert!(view.contains("╭ Retrying in"), "{view}");
        assert!(view.contains("keep draft"), "{view}");
        assert!(
            view.lines().any(|line| line.starts_with("  Toasting ▾")),
            "{view}"
        );
        assert!(view.contains("Peer Agent Toasting ▸"), "{view}");
        observe_root_event(&mut chat, EventKind::ProviderRetryStarted);
        let resumed = render(&mut chat, 40, 20)?;
        assert!(
            resumed.contains(&format!("  {mini} Toasting ▾")),
            "{resumed}"
        );
        assert!(
            resumed.contains(&format!("╭ {full}Toasting ─")),
            "{resumed}"
        );
        assert_eq!(chat.tick(Duration::from_nanos(1)), !reduced_motion);
        let label = if reduced_motion {
            "Toasting"
        } else {
            "Blending"
        };
        let full = if reduced_motion { " [###] " } else { " (o|o) " };
        let mini = if reduced_motion { "[#]" } else { "(o)" };
        let view = render(&mut chat, 40, 20)?;
        assert!(view.contains(&format!("╭ {full}{label} ─")), "{view}");
        assert!(view.contains(&format!("  {mini} {label} ▾")), "{view}");
        assert!(view.contains(&format!("Peer Agent {label} ▸")), "{view}");
        assert_eq!(view.matches(label).count(), 3, "{view}");
        assert_eq!(
            chat.tick(Duration::from_nanos(5_759_999_999)),
            !reduced_motion
        );
        chat.provider_paused("Provider request failed.");
        let paused = render(&mut chat, 40, 20)?;
        assert!(paused.contains("╭ Paused ─"), "{paused}");
        assert!(
            paused.contains(&format!("Peer Agent {label} ▸")),
            "{paused}"
        );
        assert!(
            paused
                .lines()
                .any(|line| line.starts_with(&format!("  {label} ▾"))),
            "{paused}"
        );
        assert!(!chat.tick(Duration::MAX));
        assert_eq!(paused, render(&mut chat, 40, 20)?);
        for glyph in [
            "(o|o)", "(|||)", "[|||]", "[:::]", "[###]", "(o)", "(|)", "[|]", "[:]", "[#]",
        ] {
            assert!(!paused.contains(glyph), "{paused}");
        }
        chat.provider_resumed();
        let full = if reduced_motion { " [###] " } else { " (|||) " };
        let mini = if reduced_motion { "[#]" } else { "(|)" };
        let resumed = render(&mut chat, 40, 20)?;
        assert!(
            resumed.contains(&format!("  {mini} {label} ▾")),
            "{resumed}"
        );
        assert!(resumed.contains(&format!("╭ {full}{label} ─")), "{resumed}");
        assert_eq!(chat.tick(Duration::from_nanos(1)), !reduced_motion);
        let label = if reduced_motion {
            "Toasting"
        } else {
            "Refining"
        };
        let full = if reduced_motion { " [###] " } else { " (o|o) " };
        let mini = if reduced_motion { "[#]" } else { "(o)" };
        let view = render(&mut chat, 40, 20)?;
        assert!(view.contains(&format!("  {mini} {label} ▾")), "{view}");
        assert!(view.contains(&format!("╭ {full}{label} ─")), "{view}");
        assert!(view.contains(&format!("Peer Agent {label} ▸")), "{view}");
        assert_eq!(chat.tick(Duration::from_millis(57_600)), !reduced_motion);
        let view = render(&mut chat, 40, 20)?;
        assert!(view.contains(&format!("  {mini} Toasting ▾")), "{view}");
        assert!(view.contains(&format!("╭ {full}Toasting ─")), "{view}");
        assert!(view.contains("Peer Agent Toasting ▸"), "{view}");
        chat.stopping();
        let stopping = render(&mut chat, 40, 20)?;
        assert!(stopping.contains("Stopping..."), "{stopping}");
        assert!(!stopping.contains(full), "{stopping}");
        assert!(!stopping.contains(mini), "{stopping}");
        assert!(!chat.tick(Duration::from_secs(10)));
        assert_eq!(render(&mut chat, 40, 20)?, stopping);
        assert!(!chat.tick(Duration::MAX));
        assert_eq!(render(&mut chat, 40, 20)?, stopping);
        chat.observe_agent(&AgentUpdate {
            session: 10,
            agent: fixture_agent("Peer"),
            name: "Peer".into(),
            kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
        });
        chat.complete("done");
        let completed = render(&mut chat, 40, 20)?;
        assert!(!chat.tick(Duration::from_secs(10)));
        assert_eq!(completed, render(&mut chat, 40, 20)?);
        assert_eq!(
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit("keep draft".into())
        );
        let restarted = render(&mut chat, 40, 20)?;
        assert!(
            restarted.contains(&format!("  {mini} Toasting")),
            "{restarted}"
        );
        assert!(
            restarted.contains(&format!("╭ {full}Toasting ─")),
            "{restarted}"
        );
    }
    Ok(())
}

#[test]
fn cancellation_gestures_observe_deadlines_and_preserve_busy_drafts() -> Result<(), Box<dyn Error>>
{
    let now = std::time::Instant::now();
    for delay in [999, 1000, 1001] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "run");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        type_text(&mut chat, "keep this draft");
        assert_eq!(
            chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
            Action::Redraw
        );
        let view = render(&mut chat, 64, 16)?;
        assert!(
            view.lines()
                .last()
                .is_some_and(|line| line.starts_with("╰ Esc again to stop ")),
            "{view}"
        );
        assert_eq!(
            chat.input_at(
                key(KeyCode::Esc, KeyModifiers::NONE),
                now + Duration::from_millis(delay)
            ),
            if delay < 1000 {
                Action::Cancel
            } else {
                Action::Redraw
            }
        );
        if delay < 1000 {
            let view = render(&mut chat, 64, 16)?;
            assert!(
                view.lines()
                    .last()
                    .is_some_and(|line| line.starts_with("╰ Stopping... ")),
                "{view}"
            );
            assert!(view.contains("keep this draft"), "{view}");
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::None
            );
            chat.cancelled();
            let view = render(&mut chat, 64, 16)?;
            assert!(view.contains("run"), "{view}");
            assert!(view.contains("keep this draft"), "{view}");
            for forbidden in ["Error:", "cancelled", "runtime reset", "effects retained"] {
                assert!(!view.contains(forbidden), "{view}");
            }
            assert_eq!(
                chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit("keep this draft".into())
            );
        }
    }
    for busy in [false, true] {
        for draft in ["", "draft"] {
            for delay in [2999, 3000, 3001] {
                let mut chat = Chat::new(64)?;
                if busy {
                    type_text(&mut chat, "run");
                    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
                }
                type_text(&mut chat, draft);
                assert_eq!(
                    chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now),
                    if busy && draft.is_empty() {
                        Action::Cancel
                    } else {
                        Action::Redraw
                    }
                );
                let view = render(&mut chat, 64, 16)?;
                assert!(
                    view.lines().last().is_some_and(|line| line.starts_with(
                        if busy && draft.is_empty() {
                            "╰ Stopping... · Ctrl+C to close "
                        } else {
                            "╰ Ctrl+C to close "
                        }
                    )),
                    "{view}"
                );
                assert!(!view.contains("│  draft"), "{view}");
                assert_eq!(
                    chat.input_at(
                        key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                        now + Duration::from_millis(delay)
                    ),
                    if delay < 3000 {
                        Action::Exit
                    } else if busy && !draft.is_empty() {
                        Action::Cancel
                    } else {
                        Action::Redraw
                    }
                );
            }
        }
    }
    for escape_first in [false, true] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "run");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        type_text(&mut chat, "draft");
        if escape_first {
            chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
        }
        chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now);
        let view = render(&mut chat, 64, 16)?;
        assert!(
            view.lines()
                .last()
                .is_some_and(|line| line.starts_with("╰ Ctrl+C to close ")),
            "{view}"
        );
        assert_eq!(
            chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
            if escape_first {
                Action::Cancel
            } else {
                Action::Redraw
            }
        );
        let view = render(&mut chat, 64, 16)?;
        assert!(
            view.lines()
                .last()
                .is_some_and(|line| line.starts_with(if escape_first {
                    "╰ Stopping... · Ctrl+C to close "
                } else {
                    "╰ Esc again to stop "
                })),
            "{view}"
        );
        assert_eq!(
            chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now),
            Action::Exit
        );
    }
    Ok(())
}

#[test]
fn cancellation_confirmation_ignores_repeats_and_resets_on_editing_and_focus()
-> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    for code in [KeyCode::Esc, KeyCode::Char('c')] {
        let modifiers = if code == KeyCode::Esc {
            KeyModifiers::NONE
        } else {
            KeyModifiers::CONTROL
        };
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "run");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert_eq!(
                chat.input_at(
                    Event::Key(KeyEvent::new_with_kind(code, modifiers, kind)),
                    now
                ),
                Action::None
            );
        }
        let action = chat.input_at(key(code, modifiers), now);
        assert_eq!(
            action,
            if code == KeyCode::Esc {
                Action::Redraw
            } else {
                Action::Cancel
            }
        );
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            assert_eq!(
                chat.input_at(
                    Event::Key(KeyEvent::new_with_kind(code, modifiers, kind)),
                    now + Duration::from_millis(500)
                ),
                Action::None
            );
        }
        assert_eq!(
            chat.input_at(key(code, modifiers), now + Duration::from_millis(600)),
            if code == KeyCode::Esc {
                Action::Cancel
            } else {
                Action::Exit
            }
        );
    }
    for reset in [
        key(KeyCode::Char('x'), KeyModifiers::NONE),
        key(KeyCode::Enter, KeyModifiers::NONE),
        Event::FocusLost,
        Event::FocusGained,
    ] {
        let mut chat = Chat::new(64)?;
        type_text(&mut chat, "run");
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
        chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
        chat.input_at(reset.clone(), now);
        assert_eq!(
            chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
            Action::Redraw
        );
        chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now);
        chat.input_at(reset, now);
        assert_ne!(
            chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now),
            Action::Exit
        );
    }
    let mut idle = Chat::new(64)?;
    type_text(&mut idle, "untouched");
    for _ in 0..2 {
        assert_eq!(
            idle.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
            Action::None
        );
    }
    assert_eq!(
        idle.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("untouched".into())
    );
    Ok(())
}

#[test]
fn popup_escape_does_not_arm_cancellation_and_stopping_outlives_the_exit_hint()
-> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("file.rs"), "fixture")?;
    let mut chat = Chat::new(64)?;
    chat.set_working_directory(directory.path());
    type_text(&mut chat, "run");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    type_text(&mut chat, "@");
    settle_files(&mut chat)?;
    let now = std::time::Instant::now();
    assert!(render(&mut chat, 64, 16)?.contains("file.rs"));
    assert_eq!(
        chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
        Action::Redraw
    );
    assert!(!render(&mut chat, 64, 16)?.contains("Esc again"));
    assert_eq!(
        chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
        Action::Redraw
    );
    assert_eq!(
        chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now),
        Action::Cancel
    );
    chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now);
    for width in [64, 24, 12] {
        let view = render(&mut chat, width, 16)?;
        let bottom = view
            .lines()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        assert!(bottom.contains("Stopping..."), "{view}");
        assert!(!view.contains("Toasting"), "{view}");
    }
    assert!(chat.tick_at(Duration::ZERO, now + Duration::from_secs(3)));
    let view = render(&mut chat, 64, 16)?;
    assert!(
        view.lines()
            .last()
            .is_some_and(|line| line.starts_with("╰ Stopping... ")),
        "{view}"
    );
    assert!(!view.contains("Ctrl+C to close"), "{view}");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 16)?;
    assert!(
        view.lines()
            .last()
            .is_some_and(|line| line.starts_with("╰ Stopping... ")),
        "{view}"
    );
    Ok(())
}

#[test]
fn cancellation_hints_share_the_composer_border_without_moving_the_draft()
-> Result<(), Box<dyn Error>> {
    let now = std::time::Instant::now();
    let mut chat = Chat::new(64)?;
    chat.set_context("~/project", "medium");
    type_text(&mut chat, "run");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    type_text(&mut chat, "preserved draft");
    let before = render(&mut chat, 64, 16)?;
    let draft_row = before
        .lines()
        .position(|line| line.contains("preserved draft"))
        .ok_or("missing draft before cancellation")?;
    for _ in 0..2 {
        chat.input_at(key(KeyCode::Esc, KeyModifiers::NONE), now);
        let view = render(&mut chat, 64, 16)?;
        assert_eq!(
            view.lines()
                .position(|line| line.contains("preserved draft"))
                .ok_or("missing draft during cancellation")?,
            draft_row
        );
        let border = view.lines().last().ok_or("missing composer border")?;
        assert!(
            border.starts_with("╰ Esc again to stop ") || border.starts_with("╰ Stopping... "),
            "{view}"
        );
        assert!(border.ends_with(" ~/project ╯"), "{view}");
    }
    chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now);
    let view = render(&mut chat, 64, 16)?;
    assert!(
        view.lines()
            .last()
            .is_some_and(|line| line.starts_with("╰ Stopping... · Ctrl+C to close ")
                && line.ends_with(" ~/project ╯")),
        "{view}"
    );
    for width in [24, 12] {
        let view = render(&mut chat, width, 16)?;
        let bottom = view
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(bottom.contains("Stopping..."), "{view}");
        assert!(bottom.contains("Ctrl+C to close"), "{view}");
    }
    for width in [18, 19, 24] {
        let mut chat = Chat::new(64)?;
        chat.set_context("~/long-project-directory", "medium");
        chat.input_at(key(KeyCode::Char('c'), KeyModifiers::CONTROL), now);
        let view = render(&mut chat, width, 16)?;
        assert_eq!(view.matches("Ctrl+C to close").count(), 1, "{view}");
        assert_eq!(
            view.lines()
                .last()
                .is_some_and(|line| line.starts_with("╰ Ctrl+C to close ") && line.ends_with('╯')),
            width >= 19,
            "{view}"
        );
    }
    Ok(())
}

#[test]
fn work_disclosure_preserves_edits_without_assistant_commentary() -> Result<(), Box<dyn Error>> {
    use cocoa_core::capability::{ActivityOutput, ActivityOutputKind};

    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "update the file");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    observe_root_event(
        &mut chat,
        EventKind::AssistantText {
            text: "commentary-body".into(),
            truncated: false,
        },
    );
    observe_root_event(&mut chat, EventKind::ModelFinished { executing: true });
    observe_root_event(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Edit example.rs".into(),
            text: "edit input".into(),
            summary: None,
            truncated: false,
        },
    );
    observe_root_event(
        &mut chat,
        EventKind::CapabilityOutput {
            operation: OperationId(1),
            output: ActivityOutput {
                text: "-old-value\n+new-value".into(),
                kind: ActivityOutputKind::Diff,
                summary: None,
                truncated: false,
            },
        },
    );
    observe_root_event(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Completed,
        },
    );
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Toasting"), "{view}");
    assert!(!view.contains("Assistant commentary"), "{view}");
    assert!(view.contains("Edited example.rs"), "{view}");
    assert!(!view.contains("commentary-body"), "{view}");
    assert!(!view.contains("new-value"), "{view}");
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("+new-value"), "{view}");
    assert!(view.contains("-old-value"), "{view}");
    assert!(!view.contains("commentary-body"), "{view}");
    observe_agent_event(
        &mut chat,
        10,
        "Alpha",
        EventKind::AssistantText {
            text: "child-commentary-body".into(),
            truncated: false,
        },
    );
    observe_agent_event(
        &mut chat,
        10,
        "Alpha",
        EventKind::ModelFinished { executing: true },
    );
    observe_agent_event(
        &mut chat,
        10,
        "Alpha",
        EventKind::AnswerCompleted {
            text: "Child result.".into(),
            truncated: false,
        },
    );
    click_activity(&mut chat, "Alpha Agent")?;
    click_activity(&mut chat, "Agent answer")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Child result."), "{view}");
    assert!(view.contains("+new-value"), "{view}");
    assert!(!view.contains("Assistant commentary"), "{view}");
    assert!(!view.contains("commentary-body"), "{view}");
    chat.complete("Changes applied.");
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Changes applied."), "{view}");
    assert!(!view.contains("Assistant commentary"), "{view}");
    assert!(!view.contains("commentary-body"), "{view}");
    Ok(())
}

#[test]
fn work_finishes_only_after_the_root_and_every_agent_settle() -> Result<(), Box<dyn Error>> {
    for root_first in [false, true] {
        for status in [
            ActivityStatus::Completed,
            ActivityStatus::Failed { exit_code: Some(1) },
            ActivityStatus::Unknown,
        ] {
            let mut chat = Chat::new(128)?;
            type_text(&mut chat, "delegate");
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            observe_root_event(&mut chat, EventKind::ModelStarted { round: 1 });
            for (session, name) in [(10, "Alpha"), (10, "Beta")] {
                chat.observe_agent(&AgentUpdate {
                    session,
                    agent: fixture_agent(name),
                    name: name.into(),
                    kind: AgentUpdateKind::Started,
                });
            }
            let view = render(&mut chat, 64, 32)?;
            assert!(view.contains("(o) Alpha Agent Toasting"), "{view}");
            assert!(view.contains("(o) Beta Agent Toasting"), "{view}");
            assert!(!view.contains("Work Done"), "{view}");
            chat.observe_agent(&AgentUpdate {
                session: 10,
                agent: fixture_agent("Alpha"),
                name: "Alpha".into(),
                kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
            });
            if root_first {
                chat.complete("Root answer.");
            }
            let view = render(&mut chat, 64, 32)?;
            assert!(view.contains("Alpha Agent Worked"), "{view}");
            assert!(!view.contains("(o) Alpha Agent"), "{view}");
            assert!(view.contains("(o) Beta Agent Toasting"), "{view}");
            assert!(!view.contains("Work Done"), "{view}");
            assert!(view.contains("(o) Toasting ▾"), "{view}");
            assert_eq!(view.matches("(o)").count(), 2, "{view}");
            assert_eq!(view.contains(" (o|o) "), !root_first, "{view}");
            assert!(chat.tick(Duration::from_millis(480)));
            let view = render(&mut chat, 64, 32)?;
            assert!(view.contains("(|) Toasting ▾"), "{view}");
            assert!(view.contains("(|) Beta Agent Toasting"), "{view}");
            assert_eq!(view.matches("(|)").count(), 2, "{view}");
            assert_eq!(view.contains(" (|||) "), !root_first, "{view}");
            chat.observe_agent(&AgentUpdate {
                session: 10,
                agent: fixture_agent("Beta"),
                name: "Beta".into(),
                kind: AgentUpdateKind::Finished(status),
            });
            let view = render(&mut chat, 64, 32)?;
            let agent_label = if matches!(status, ActivityStatus::Failed { .. }) {
                "Beta Agent stopped"
            } else {
                "Beta Agent Worked"
            };
            assert_eq!(view.contains(agent_label), !root_first, "{view}");
            assert_eq!(view.contains("Work Done ▸"), root_first, "{view}");
            if !root_first {
                assert!(view.contains("Toasting"), "{view}");
                chat.complete("Root answer.");
            }
            let view = render(&mut chat, 64, 32)?;
            assert!(view.contains("Work Done"), "{view}");
            assert!(!view.contains("Toasting"), "{view}");
            assert!(!view.contains("(|||)"), "{view}");
            assert!(!view.contains("(|)"), "{view}");
            assert!(!chat.tick(Duration::from_millis(480)));
            assert!(view.contains("Root answer."), "{view}");
        }
    }
    Ok(())
}

#[test]
fn work_waits_for_retained_tools_and_accepts_late_output_while_collapsed()
-> Result<(), Box<dyn Error>> {
    use cocoa_core::capability::{ActivityOutput, ActivityOutputKind};

    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "start a command");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    observe_root_event(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Background command".into(),
            text: "command input".into(),
            summary: None,
            truncated: false,
        },
    );
    observe_root_event(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Running,
        },
    );
    chat.complete("Root answer.");
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Toasting"), "{view}");
    assert!(!view.contains("Work Done"), "{view}");
    assert!(view.contains("(o) Toasting ▾"), "{view}");
    assert_eq!(view.matches("(o)").count(), 1, "{view}");
    assert!(!view.contains(" (o|o) "), "{view}");
    assert!(chat.tick(Duration::from_millis(480)));
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("(|) Toasting ▾"), "{view}");
    assert_eq!(view.matches("(|)").count(), 1, "{view}");
    assert!(!view.contains(" (|||) "), "{view}");
    click_activity(&mut chat, "Toasting")?;
    observe_root_event(
        &mut chat,
        EventKind::CapabilityOutput {
            operation: OperationId(1),
            output: ActivityOutput {
                text: "late-root-output".into(),
                kind: ActivityOutputKind::Command,
                summary: None,
                truncated: false,
            },
        },
    );
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Toasting"), "{view}");
    assert!(!view.contains("Background command"), "{view}");
    assert!(!view.contains("late-root-output"), "{view}");
    observe_root_event(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Completed,
        },
    );
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Work Done"), "{view}");
    assert!(view.contains("Root answer."), "{view}");
    assert!(!view.contains("(|||)"), "{view}");
    assert!(!view.contains("(|)"), "{view}");
    assert!(!chat.tick(Duration::from_millis(480)));
    assert!(!view.contains("Background command"), "{view}");
    click_activity(&mut chat, "Work Done")?;
    click_activity(&mut chat, "Background command")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("late-root-output"), "{view}");
    assert!(!view.contains("command input"), "{view}");
    Ok(())
}

#[test]
fn agent_observations_after_root_completion_stay_in_their_work_group() -> Result<(), Box<dyn Error>>
{
    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "delegate");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    observe_root_event(&mut chat, EventKind::ModelStarted { round: 1 });
    chat.observe_agent(&AgentUpdate {
        session: 10,
        agent: fixture_agent("Late"),
        name: "Late".into(),
        kind: AgentUpdateKind::Started,
    });
    chat.complete("Root answer.");
    observe_agent_event(
        &mut chat,
        10,
        "Late",
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Late command".into(),
            text: "late-detail".into(),
            summary: None,
            truncated: false,
        },
    );
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Late Agent Toasting"), "{view}");
    assert!(view.contains("Root answer."), "{view}");
    assert!(!view.contains("Work Done"), "{view}");
    assert!(!view.contains("Late command"), "{view}");
    click_activity(&mut chat, "Late Agent Toasting")?;
    click_activity(&mut chat, "Late command")?;
    assert!(render(&mut chat, 64, 32)?.contains("late-detail"));
    observe_agent_event(
        &mut chat,
        10,
        "Late",
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Completed,
        },
    );
    chat.observe_agent(&AgentUpdate {
        session: 10,
        agent: fixture_agent("Late"),
        name: "Late".into(),
        kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
    });
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Work Done ▸"), "{view}");
    assert!(!view.contains("Late Agent Worked"), "{view}");
    assert!(!view.contains("late-detail"), "{view}");
    click_activity(&mut chat, "Work Done")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Late Agent Worked"), "{view}");
    assert!(view.contains("late-detail"), "{view}");
    click_activity(&mut chat, "Work Done")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Root answer."), "{view}");
    assert!(!view.contains("Late Agent"), "{view}");
    assert!(!view.contains("late-detail"), "{view}");
    Ok(())
}

#[test]
fn same_named_agents_settle_by_identity_without_hiding_other_work() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    chat.initial_prompt("delegate")?;
    observe_root_event(&mut chat, EventKind::ModelStarted { round: 1 });
    for (session, identity) in [(10, 1), (10, 2), (11, 1)] {
        chat.observe_agent(&AgentUpdate {
            session,
            agent: cocoa_core::store::AgentId::try_from(identity)?,
            name: "Reviewer".into(),
            kind: AgentUpdateKind::Started,
        });
    }
    let view = render(&mut chat, 80, 24)?;
    assert_eq!(view.matches("Reviewer Agent Toasting").count(), 3, "{view}");
    for (session, identity) in [(10, 1), (11, 1)] {
        chat.observe_agent(&AgentUpdate {
            session,
            agent: cocoa_core::store::AgentId::try_from(identity)?,
            name: "Reviewer".into(),
            kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
        });
        assert!(chat.has_active_agents());
    }
    chat.observe_agent(&AgentUpdate {
        session: 10,
        agent: cocoa_core::store::AgentId::try_from(2)?,
        name: "Reviewer".into(),
        kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
    });
    assert!(!chat.has_active_agents());
    Ok(())
}

#[test]
fn agent_invocations_and_tool_ids_do_not_collide_or_replace_the_root_answer()
-> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "delegate");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    observe_root_event(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "Root command".into(),
            text: "root-detail".into(),
            summary: None,
            truncated: false,
        },
    );
    for (session, name, title, detail) in [
        (10, "Alpha", "Alpha command", "alpha-detail"),
        (10, "Beta", "Beta command", "beta-detail"),
    ] {
        chat.observe_agent(&AgentUpdate {
            session,
            agent: fixture_agent(name),
            name: name.into(),
            kind: AgentUpdateKind::Started,
        });
        observe_agent_event(
            &mut chat,
            session,
            name,
            EventKind::CapabilityInput {
                operation: OperationId(1),
                title: title.into(),
                text: detail.into(),
                summary: None,
                truncated: false,
            },
        );
        for kind in [
            EventKind::AssistantText {
                text: format!("{name}-private-stream"),
                truncated: false,
            },
            EventKind::AnswerCompleted {
                text: format!("{name}-private-answer"),
                truncated: false,
            },
        ] {
            observe_agent_event(&mut chat, session, name, kind);
            let view = render(&mut chat, 64, 32)?;
            assert!(view.contains("Root command"), "{view}");
            for hidden in [
                "Alpha command",
                "Beta command",
                "private-stream",
                "private-answer",
            ] {
                assert!(!view.contains(hidden), "{view}");
            }
        }
    }
    click_activity(&mut chat, "Root command")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("root-detail"), "{view}");
    assert!(!view.contains("alpha-detail"), "{view}");
    assert!(!view.contains("beta-detail"), "{view}");
    click_activity(&mut chat, "Alpha Agent Toasting")?;
    click_activity(&mut chat, "Alpha command")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("root-detail"), "{view}");
    assert!(view.contains("alpha-detail"), "{view}");
    assert!(!view.contains("Beta command"), "{view}");
    assert!(!view.contains("beta-detail"), "{view}");
    click_activity(&mut chat, "Beta Agent Toasting")?;
    click_activity(&mut chat, "Beta command")?;
    let view = render(&mut chat, 64, 32)?;
    for visible in [
        "Root command",
        "root-detail",
        "Alpha command",
        "alpha-detail",
        "Beta command",
        "beta-detail",
    ] {
        assert!(view.contains(visible), "{view}");
    }
    for (session, name) in [(10, "Alpha"), (10, "Beta")] {
        chat.observe_agent(&AgentUpdate {
            session,
            agent: fixture_agent(name),
            name: name.into(),
            kind: AgentUpdateKind::Finished(ActivityStatus::Completed),
        });
    }
    chat.complete("Authoritative root answer.");
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Authoritative root answer."), "{view}");
    for hidden in [
        "Root command",
        "root-detail",
        "Alpha",
        "Beta",
        "private-stream",
        "private-answer",
    ] {
        assert!(!view.contains(hidden), "{view}");
    }
    Ok(())
}

#[test]
fn collapsed_agents_hide_descendants_from_keyboard_and_mouse() -> Result<(), Box<dyn Error>> {
    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "delegate");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    observe_root_event(&mut chat, EventKind::ModelStarted { round: 1 });
    chat.observe_agent(&AgentUpdate {
        session: 10,
        agent: fixture_agent("Reviewer"),
        name: "Reviewer".into(),
        kind: AgentUpdateKind::Started,
    });
    for (operation, title, text) in [
        (OperationId(1), "First child", "first-private-detail"),
        (OperationId(2), "Second child", "second-private-detail"),
    ] {
        observe_agent_event(
            &mut chat,
            10,
            "Reviewer",
            EventKind::CapabilityInput {
                operation,
                title: title.into(),
                text: text.into(),
                summary: None,
                truncated: false,
            },
        );
    }
    type_text(&mut chat, "next draft");
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Reviewer Agent Toasting"), "{view}");
    assert!(!view.contains("First child"), "{view}");
    assert!(!view.contains("Second child"), "{view}");
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    for _ in 0..4 {
        chat.input(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    }
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("First child"), "{view}");
    assert!(view.contains("Second child"), "{view}");
    assert!(!view.contains("private-detail"), "{view}");
    chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("first-private-detail"), "{view}");
    assert!(!view.contains("second-private-detail"), "{view}");
    chat.input(key(KeyCode::Left, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 32)?;
    let child_row = view
        .lines()
        .position(|line| line.contains("Second child"))
        .ok_or("missing second child")? as u16;
    click_activity(&mut chat, "Reviewer Agent Toasting")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(!view.contains("child"), "{view}");
    chat.input(mouse(
        MouseEventKind::Down(MouseButton::Left),
        (10, child_row),
    ));
    chat.input(mouse(
        MouseEventKind::Up(MouseButton::Left),
        (10, child_row),
    ));
    click_activity(&mut chat, "Reviewer Agent Toasting")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Second child"), "{view}");
    assert!(!view.contains("private-detail"), "{view}");
    click_activity(&mut chat, "Toasting")?;
    let view = render(&mut chat, 64, 32)?;
    assert!(!view.contains("Reviewer"), "{view}");
    assert!(!view.contains("child"), "{view}");
    chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
    for _ in 0..4 {
        chat.input(key(KeyCode::Down, KeyModifiers::NONE));
    }
    chat.input(key(KeyCode::Right, KeyModifiers::NONE));
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("Reviewer Agent Toasting"), "{view}");
    assert!(view.contains("Second child"), "{view}");
    assert!(!view.contains("private-detail"), "{view}");
    chat.input(key(KeyCode::Esc, KeyModifiers::NONE));
    chat.complete("Root answer.");
    type_text(&mut chat, "!");
    assert_eq!(
        chat.input(key(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Submit("next draf!t".into())
    );
    Ok(())
}

#[test]
fn accepted_spawn_rows_are_replaced_by_agents_in_either_event_order() -> Result<(), Box<dyn Error>>
{
    for (name, label) in [
        ("Reviewer", "Reviewer Agent"),
        ("sub-agent", "Sub Agent"),
        ("code_review", "Code Review Agent"),
        ("--mIXed__case-", "Mixed Case Agent"),
        ("agent", "Agent"),
        ("agent007", "Agent007 Agent"),
        ("---", "--- Agent"),
    ] {
        for agent_first in [false, true] {
            let mut chat = Chat::new(128)?;
            type_text(&mut chat, "delegate");
            chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
            observe_root_event(&mut chat, EventKind::ModelStarted { round: 1 });
            for started in [agent_first, !agent_first] {
                if started {
                    chat.observe_agent(&AgentUpdate {
                        session: 10,
                        agent: fixture_agent(name),
                        name: name.into(),
                        kind: AgentUpdateKind::Started,
                    });
                } else {
                    observe_root_event(
                        &mut chat,
                        EventKind::CapabilityInput {
                            operation: OperationId(1),
                            title: "cocoa.spawn".into(),
                            text: name.into(),
                            summary: None,
                            truncated: false,
                        },
                    );
                }
            }
            observe_root_event(
                &mut chat,
                EventKind::CapabilityFinished {
                    operation: OperationId(1),
                    status: ActivityStatus::Completed,
                },
            );
            let view = render(&mut chat, 64, 32)?;
            assert_eq!(
                view.matches(&format!("(o) {label} Toasting ▸")).count(),
                1,
                "{view}"
            );
            assert!(!view.contains("cocoa.spawn"), "{view}");
            chat.input(key(KeyCode::Tab, KeyModifiers::NONE));
            chat.input(key(KeyCode::Right, KeyModifiers::NONE));
            let view = render(&mut chat, 64, 32)?;
            assert!(!view.contains("cocoa.spawn"), "{view}");
        }
    }
    let mut chat = Chat::new(128)?;
    type_text(&mut chat, "delegate");
    chat.input(key(KeyCode::Enter, KeyModifiers::NONE));
    observe_root_event(
        &mut chat,
        EventKind::CapabilityInput {
            operation: OperationId(1),
            title: "cocoa.spawn".into(),
            text: "Rejected".into(),
            summary: None,
            truncated: false,
        },
    );
    observe_root_event(
        &mut chat,
        EventKind::CapabilityFinished {
            operation: OperationId(1),
            status: ActivityStatus::Failed { exit_code: None },
        },
    );
    let view = render(&mut chat, 64, 32)?;
    assert!(view.contains("cocoa.spawn"), "{view}");
    assert!(!view.contains("Rejected Agent"), "{view}");
    click_activity(&mut chat, "cocoa.spawn")?;
    assert!(render(&mut chat, 64, 32)?.contains("Rejected"));
    Ok(())
}

fn observe_root_event(chat: &mut Chat, kind: EventKind) {
    chat.observe(&Observation::Event(EngineEvent {
        invocation: InvocationId(1),
        kind,
    }));
}

fn observe_agent_event(chat: &mut Chat, session: u64, name: &str, kind: EventKind) {
    chat.observe_agent(&AgentUpdate {
        session,
        agent: fixture_agent(name),
        name: name.into(),
        kind: AgentUpdateKind::Observation(Observation::Event(EngineEvent {
            invocation: InvocationId(1),
            kind,
        })),
    });
}

fn fixture_agent(name: &str) -> cocoa_core::store::AgentId {
    match cocoa_core::store::AgentId::try_from(if name == "Alpha" {
        1
    } else if name == "Beta" {
        2
    } else {
        3
    }) {
        Ok(agent) => agent,
        Err(_) => unreachable!("fixture identities are positive"),
    }
}

fn click_activity(chat: &mut Chat, label: &str) -> Result<(), Box<dyn Error>> {
    let view = render(chat, 64, 32)?;
    let row = view
        .lines()
        .position(|line| line.contains(label))
        .ok_or_else(|| format!("missing activity {label}: {view}"))? as u16;
    chat.input(mouse(MouseEventKind::Down(MouseButton::Left), (10, row)));
    chat.input(mouse(MouseEventKind::Up(MouseButton::Left), (10, row)));
    Ok(())
}

fn clipboard_image(pixel: [u8; 4]) -> Result<Image, Box<dyn Error>> {
    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(&pixel, 1, 1, ExtendedColorType::Rgba8)?;
    Ok(Image::from_png(&png)?)
}

fn type_text(chat: &mut Chat, text: &str) {
    for character in text.chars() {
        let event = if character == '\n' {
            key(KeyCode::Char('j'), KeyModifiers::CONTROL)
        } else {
            key(KeyCode::Char(character), KeyModifiers::NONE)
        };
        chat.input(event);
    }
}

fn settle_files(chat: &mut Chat) -> Result<(), Box<dyn Error>> {
    for _ in 0..1000 {
        chat.tick(Duration::from_millis(40));
        let view = render(chat, 64, 16)?;
        if !view.contains("searching") && !view.contains("Searching") {
            return Ok(());
        }
    }
    Err("file discovery did not settle".into())
}

fn render(chat: &mut Chat, width: u16, height: u16) -> Result<String, Box<dyn Error>> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| chat.render(frame))?;
    Ok(terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn mouse(kind: MouseEventKind, (column, row): (u16, u16)) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}
