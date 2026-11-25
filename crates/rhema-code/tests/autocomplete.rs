//! The chat completion palette's key rules: Tab completes, Enter takes
//! the highlighted row, commands live on one line, and a multi-line
//! prompt keeps its arrows for the cursor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rhema::commands::{Parsed, parse};
use rhema::icons::UNICODE;
use rhema::screens::{ChatAction, ChatState, filtered_commands, palette_rows};
use rhema_provider::ModelCard;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn model(provider: &'static str, id: &str) -> rhema::screens::CatalogModel {
    rhema::screens::CatalogModel {
        provider,
        instance: None,
        model: ModelCard {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: String::new(),
            input_token_limit: None,
            output_token_limit: None,
            supports_vision: None,
            generation_methods: vec!["generateContent".to_owned()],
            pricing: None,
            family: None,
            release_date: None,
        },
    }
}

fn type_text(chat: &mut ChatState, text: &str) {
    for c in text.chars() {
        chat.handle_key(key(KeyCode::Char(c)), &[], &[], &[], &UNICODE);
    }
}

#[test]
fn tab_on_a_command_opens_its_arguments() {
    let mut chat = ChatState::new();
    type_text(&mut chat, "/");
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &UNICODE);
    assert_eq!(chat.input, "/model ");
    assert_eq!(chat.cursor, "/model ".chars().count());
}

#[test]
fn tab_on_a_no_argument_command_leaves_the_whole_name() {
    let mut chat = ChatState::new();
    type_text(&mut chat, "/cl");
    chat.handle_key(key(KeyCode::Tab), &[], &[], &[], &UNICODE);
    assert_eq!(chat.input, "/clear");
    // The exact name is the whole command: no row is left to show.
    assert!(filtered_commands("/clear").is_empty());
}

#[test]
fn a_prefix_with_siblings_keeps_its_exact_row_first() {
    let names: Vec<&str> = filtered_commands("/r").iter().map(|s| s.name).collect();
    assert_eq!(names, ["/r", "/retry", "/resume"]);
}

#[test]
fn enter_takes_the_highlighted_command_row() {
    let mut chat = ChatState::new();
    type_text(&mut chat, "/");
    chat.handle_key(key(KeyCode::Down), &[], &[], &[], &UNICODE);
    assert_eq!(chat.palette_index, 1);
    match chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &UNICODE) {
        ChatAction::Submit { text, .. } => assert_eq!(text, "/switch"),
        other => panic!("expected the highlighted row, got {other:?}"),
    }
}

#[test]
fn enter_takes_the_highlighted_argument_row() {
    let models = vec![
        model("google", "gemini-3.8-pro"),
        model("openrouter", "auto"),
    ];
    let mut chat = ChatState::new();
    type_text(&mut chat, "/model ");
    chat.handle_key(key(KeyCode::Down), &models, &[], &[], &UNICODE);
    match chat.handle_key(key(KeyCode::Enter), &models, &[], &[], &UNICODE) {
        ChatAction::Submit { text, .. } => {
            assert_eq!(text, "/model openrouter/auto")
        }
        other => panic!("expected the highlighted model, got {other:?}"),
    }
}

#[test]
fn enter_on_a_path_row_splices_instead_of_submitting() {
    let dir = tempfile::tempdir().expect("a scratch dir");
    std::fs::write(dir.path().join("alpha.txt"), "").expect("a file");
    let mut chat = ChatState::new();
    chat.input = format!("see {}/al", dir.path().display());
    chat.cursor = chat.input.chars().count();
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &UNICODE),
        ChatAction::None
    );
    assert_eq!(
        chat.input,
        format!("see {}/alpha.txt", dir.path().display())
    );
}

#[test]
fn an_exact_path_leaves_no_row_and_enter_submits() {
    let dir = tempfile::tempdir().expect("a scratch dir");
    let path = dir.path().join("alpha.txt");
    std::fs::write(&path, "").expect("a file");
    let mut chat = ChatState::new();
    chat.input = format!("see {}", path.display());
    chat.cursor = chat.input.chars().count();
    assert!(palette_rows(&chat.input, chat.cursor, &[], &[], &[], &UNICODE).is_empty());
    assert_eq!(
        chat.handle_key(key(KeyCode::Enter), &[], &[], &[], &UNICODE),
        ChatAction::Submit {
            text: chat.input.clone(),
            images: Vec::new(),
            spans: Vec::new(),
        }
    );
}

#[test]
fn a_multiline_prompt_keeps_its_arrows_for_the_cursor() {
    let mut chat = ChatState::new();
    chat.input = "./src/\nsecond ./src/".to_owned();
    chat.cursor = chat.input.chars().count();
    assert!(!palette_rows(&chat.input, chat.cursor, &[], &[], &[], &UNICODE).is_empty());
    chat.handle_key(key(KeyCode::Up), &[], &[], &[], &UNICODE);
    assert_eq!(chat.cursor, 6, "Up moved a line instead of the palette");
}

#[test]
fn a_multiline_slash_input_is_prose() {
    assert!(matches!(parse("/clear\nmore"), Parsed::Prompt(_)));
    assert!(filtered_commands("/clear\nmore").is_empty());
    assert!(palette_rows("/clear\nmore", 6, &[], &[], &[], &UNICODE).is_empty());
}

#[test]
fn tab_fills_the_fully_qualified_model_argument() {
    let models = vec![model("openrouter", "openrouter/auto")];
    let mut chat = ChatState::new();
    type_text(&mut chat, "/model openrouter/auto");
    chat.handle_key(key(KeyCode::Tab), &models, &[], &[], &UNICODE);
    assert_eq!(chat.input, "/model openrouter/openrouter/auto");
}
