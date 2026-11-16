//! Opt-in, deterministic full-Chat rendering timings, not a performance assertion.
//!
//! Run without other tests or competing builds:
//! ./scripts/test --release -p cocoa --test streaming_benchmark --run-ignored only --nocapture
//!
//! Measures Terminal::draw, including Chat rendering, buffer diffing and TestBackend
//! writes. Setup, event ingestion, assertions and reporting are outside the timers.
//! TestBackend does not measure terminal I/O or production frame scheduling.

use std::{
    error::Error,
    hint::black_box,
    time::{Duration, Instant},
};

use cocoa::chat::{Action, Chat};
use cocoa_core::{
    engine::{Event as EngineEvent, EventKind, Observation},
    store::InvocationId,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers},
};

const STREAM_FRAMES: usize = 96;
const CACHE_FRAMES_PER_UPDATE: usize = 4;
const HEIGHT: u16 = 32;
const FINAL_MARKER: &str = "STREAM-END-0096";

#[test]
#[ignore = "manual release-mode rendering benchmark; prints timings without thresholds"]
fn streaming_display_benchmark() -> Result<(), Box<dyn Error>> {
    println!(
        "streaming display: optimized={}, height={HEIGHT}, stream_frames={STREAM_FRAMES},          cache_frames_per_update={CACHE_FRAMES_PER_UPDATE}; times include Terminal::draw only",
        !cfg!(debug_assertions),
    );
    for history_turns in [2, 80] {
        for width in [64, 120] {
            let mut chat = Chat::new(4096)?;
            chat.set_reduced_motion(true);
            for turn in 0..history_turns {
                submit(&mut chat, &format!("Review history turn {turn}."))?;
                chat.complete(&markdown_chunk(turn).repeat(4));
            }
            submit(&mut chat, "Stream a markdown review.")?;
            let mut terminal = Terminal::new(TestBackend::new(width, HEIGHT))?;
            for _ in 0..4 {
                terminal.draw(|frame| chat.render(frame))?;
            }

            let mut growing_markdown = String::new();
            let mut streaming = Vec::with_capacity(STREAM_FRAMES);
            let mut cache_hits = Vec::with_capacity(STREAM_FRAMES * CACHE_FRAMES_PER_UPDATE);
            for update in 0..STREAM_FRAMES {
                let mut chunk = markdown_chunk(update);
                if update + 1 == STREAM_FRAMES {
                    chunk.push('\n');
                    chunk.push_str(FINAL_MARKER);
                    chunk.push('\n');
                }
                growing_markdown.push_str(&chunk);
                assert!(
                    chunk.is_ascii(),
                    "byte-sized deltas must preserve UTF-8 boundaries"
                );
                for text in chunk.as_bytes().chunks(37) {
                    chat.observe(&Observation::Event(EngineEvent {
                        invocation: InvocationId(1),
                        kind: EventKind::AssistantText {
                            text: String::from_utf8(text.to_vec())?,
                            truncated: false,
                        },
                    }));
                }
                streaming.push(draw_frame(&mut terminal, &mut chat)?);
                for _ in 0..CACHE_FRAMES_PER_UPDATE {
                    cache_hits.push(draw_frame(&mut terminal, &mut chat)?);
                }
            }
            assert_visible(&terminal, FINAL_MARKER);
            chat.complete(&growing_markdown);
            terminal.draw(|frame| chat.render(frame))?;
            assert_visible(&terminal, FINAL_MARKER);
            println!(
                "history_turns={history_turns} width={width} height={HEIGHT} markdown_bytes={}",
                growing_markdown.len(),
            );
            report("streaming", &mut streaming);
            report("cache-hit", &mut cache_hits);
        }
    }
    Ok(())
}

fn submit(chat: &mut Chat, text: &str) -> Result<(), Box<dyn Error>> {
    chat.paste_text(text)?;
    assert_eq!(
        chat.input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))),
        Action::Submit(text.into()),
    );
    Ok(())
}

fn markdown_chunk(index: usize) -> String {
    format!(
        "\n### Review {index:03}\n\n\
         A **bold** observation with *emphasis*, [a link](https://example.com/review), \
         and enough ordinary prose to wrap across narrow terminal rows.\n\n\
         - Inspect the cached layout and retain the unchanged prefix.\n\n\
         > Keep the final result visible while the answer grows.\n\n\
         \x60\x60\x60rust\nlet item = {index};\nprintln!(\"{{item}}\");\n\x60\x60\x60\n"
    )
}

fn draw_frame(
    terminal: &mut Terminal<TestBackend>,
    chat: &mut Chat,
) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    terminal.draw(|frame| black_box(&mut *chat).render(black_box(frame)))?;
    let elapsed = started.elapsed();
    black_box(terminal.backend().buffer());
    Ok(elapsed)
}

fn assert_visible(terminal: &Terminal<TestBackend>, text: &str) {
    let buffer = terminal.backend().buffer();
    let visible = buffer
        .content
        .chunks(usize::from(buffer.area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        visible.contains(text),
        "missing {text:?} in final frame:\n{visible}"
    );
}

fn report(label: &str, samples: &mut [Duration]) {
    assert!(!samples.is_empty(), "each workload must draw frames");
    let total: Duration = samples.iter().copied().sum();
    samples.sort_unstable();
    let median = samples[(samples.len() - 1) / 2];
    let p95 = samples[(samples.len() * 95).div_ceil(100) - 1];
    let maximum = samples[samples.len() - 1];
    println!(
        "  {label}: frames={} median={median:?} p95={p95:?} max={maximum:?} total={total:?}",
        samples.len(),
    );
}
