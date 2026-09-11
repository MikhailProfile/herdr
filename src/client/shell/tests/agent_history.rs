use super::*;
use crate::api::schema::{
    AgentHistoryProjectInfo, AgentHistorySessionInfo, Method, ResponseResult,
};

fn frame_text(state: &mut ClientShellState) -> String {
    let frame = state.compose(106, 30).expect("overlay frame");
    frame
        .cells
        .chunks(frame.width as usize)
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn open_overlay() -> (ClientShellState, ClientShellInput) {
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    let mut open = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::OpenAgentHistory),
        &mut open,
    );
    (state, open)
}

fn endpoint_request(outcome: &ClientShellInput) -> &crate::api::schema::Request {
    let [ClientShellAction::Endpoint { request, .. }] = &outcome.actions[..] else {
        panic!(
            "expected exactly one endpoint request, got {:?}",
            outcome.actions.len()
        );
    };
    request
}

fn session(
    id: &str,
    title: &str,
    tier: Option<crate::agent_history::MatchTier>,
) -> AgentHistorySessionInfo {
    AgentHistorySessionInfo {
        agent: "claude".into(),
        session_id: id.into(),
        title: Some(title.into()),
        title_kind: crate::agent_history::TitleKind::Custom,
        first_prompt: "first prompt".into(),
        project_path: "/Users/demo/alpha".into(),
        git_branch: "main".into(),
        first_ts_ms: 1_770_357_573_827,
        last_ts_ms: 1_770_357_700_000,
        message_count: 4,
        score: 350,
        match_tier: tier,
        snippet: None,
        open_pane_id: None,
    }
}

fn results(query: &str) -> ResponseResult {
    ResponseResult::AgentHistoryResults {
        query: query.into(),
        deep: true,
        groups: vec![
            AgentHistoryProjectInfo {
                project_path: "/Users/demo/alpha".into(),
                label: "alpha".into(),
                workspace_id: Some("ws_1".into()),
                sessions: vec![
                    session(
                        "s-1",
                        "telegram bot deploy",
                        Some(crate::agent_history::MatchTier::Title),
                    ),
                    session(
                        "s-2",
                        "review pull request",
                        Some(crate::agent_history::MatchTier::Text),
                    ),
                ],
            },
            AgentHistoryProjectInfo {
                project_path: "/Users/demo/beta".into(),
                label: "beta".into(),
                workspace_id: None,
                sessions: vec![session(
                    "s-3",
                    "zoom transcript",
                    Some(crate::agent_history::MatchTier::Prompt),
                )],
            },
        ],
    }
}

#[test]
fn opening_sends_an_initial_search_and_renders_results() {
    let (mut state, open) = open_overlay();
    assert!(open.repaint);
    let request = endpoint_request(&open);
    assert!(matches!(
        &request.method,
        Method::AgentHistorySearch(params) if params.query.is_empty() && params.deep
    ));
    assert!(matches!(
        state.overlay,
        Some(ClientShellOverlay::AgentHistory(ClientHistoryOverlay {
            search_focused: true,
            searching: true,
            ..
        }))
    ));
    let text = frame_text(&mut state);
    assert!(text.contains("searching…"), "{text}");

    let request_id = request.id.clone();
    let (repaint, actions) = state.handle_endpoint_result("boot-1", &request_id, Ok(results("")));
    assert!(repaint);
    assert!(actions.is_empty());
    let text = frame_text(&mut state);
    assert!(text.contains("alpha"), "{text}");
    assert!(text.contains("T telegram bot deploy"), "{text}");
    assert!(text.contains("~ review pull request"), "{text}");
    assert!(text.contains("P zoom transcript"), "{text}");
    assert!(text.contains("3 sessions · 2 projects"), "{text}");
    assert!(text.contains("2026-02-06 · main"), "{text}");
    assert!(text.contains("◆ alpha"), "open workspace marker: {text}");
    assert_eq!(state.hits.history_rows.len(), 5);
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    assert_eq!(
        overlay.selected,
        Some(ClientHistoryTarget::Session {
            agent: "claude".into(),
            session_id: "s-1".into()
        })
    );
    assert!(!overlay.searching);
}

#[test]
fn typing_debounces_a_new_search_and_stale_results_are_ignored() {
    let (mut state, open) = open_overlay();
    let first_id = endpoint_request(&open).id.clone();
    assert!(state.handle_input_bytes(b"tele").actions.is_empty());
    assert!(state.history_search_deadline.is_some());
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    assert_eq!(overlay.query, "tele");

    // Not yet due: nothing is sent.
    let early = state.tick_agent_history_search(std::time::Instant::now());
    assert!(early.actions.is_empty());
    // Due: the debounced request goes out as generation 2.
    let due = state
        .tick_agent_history_search(std::time::Instant::now() + std::time::Duration::from_secs(1));
    let second = endpoint_request(&due);
    assert!(matches!(&second.method, Method::AgentHistorySearch(params) if params.query == "tele"));
    let second_id = second.id.clone();
    assert!(state.history_search_deadline.is_none());

    // The newer response applies first; the older one must not overwrite it.
    let (repaint, _) = state.handle_endpoint_result("boot-1", &second_id, Ok(results("tele")));
    assert!(repaint);
    let (stale, _) = state.handle_endpoint_result(
        "boot-1",
        &first_id,
        Ok(ResponseResult::AgentHistoryResults {
            query: String::new(),
            deep: true,
            groups: Vec::new(),
        }),
    );
    assert!(!stale);
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    assert_eq!(overlay.results_query, "tele");
    assert_eq!(overlay.groups.len(), 2);
    assert!(!overlay.searching);
    let text = frame_text(&mut state);
    assert!(text.contains(" / tele"), "{text}");
}

#[test]
fn enter_resumes_the_selected_session_and_closes_on_success() {
    let (mut state, open) = open_overlay();
    let request_id = endpoint_request(&open).id.clone();
    state.handle_endpoint_result("boot-1", &request_id, Ok(results("")));

    // Leave search mode, move to the second session, resume it.
    state.handle_input_bytes(b"\x1b");
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    assert!(!overlay.search_focused);
    state.handle_input_bytes(b"j");
    let submit = state.handle_input_bytes(b"\r");
    let resume = endpoint_request(&submit);
    assert!(matches!(
        &resume.method,
        Method::AgentResume(params)
            if params.session_id == "s-2"
                && params.placement == crate::api::schema::AgentResumePlacement::Tab
                && params.focus
    ));
    let resume_id = resume.id.clone();
    let text = frame_text(&mut state);
    assert!(text.contains("resuming…"), "{text}");

    // Errors keep the overlay open and show the message.
    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &resume_id,
        Err(ClientShellEndpointError {
            code: Some("project_missing".into()),
            message: "project directory /gone does not exist".into(),
        }),
    );
    assert!(repaint);
    let text = frame_text(&mut state);
    assert!(
        text.contains("project directory /gone does not exist"),
        "{text}"
    );

    // Success closes it. `w` asks for a new workspace.
    let submit = state.handle_input_bytes(b"w");
    let resume = endpoint_request(&submit);
    assert!(matches!(
        &resume.method,
        Method::AgentResume(params)
            if params.placement == crate::api::schema::AgentResumePlacement::Workspace
    ));
    let resume_id = resume.id.clone();
    let (repaint, _) =
        state.handle_endpoint_result("boot-1", &resume_id, Ok(ResponseResult::Ok {}));
    assert!(repaint);
    assert!(state.overlay.is_none());
}

#[test]
fn space_collapses_projects_and_mouse_selects_rows() {
    let (mut state, open) = open_overlay();
    let request_id = endpoint_request(&open).id.clone();
    state.handle_endpoint_result("boot-1", &request_id, Ok(results("")));
    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"k"); // from first session up to the project row
    state.handle_input_bytes(b" ");
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    assert!(overlay.collapsed_projects.contains("/Users/demo/alpha"));
    let text = frame_text(&mut state);
    assert!(text.contains("▸ ◆ alpha"), "{text}");
    assert!(!text.contains("telegram bot deploy"), "{text}");
    assert_eq!(state.hits.history_rows.len(), 3);

    let (rect, target) = state.hits.history_rows[2].clone();
    let click = state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 4,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })]);
    assert!(
        matches!(target, ClientHistoryTarget::Session { ref session_id, .. } if session_id == "s-3")
    );
    let resume = endpoint_request(&click);
    assert!(matches!(&resume.method, Method::AgentResume(params) if params.session_id == "s-3"));

    // Clicking outside the popup closes the overlay.
    let mut state = open_overlay().0;
    state.compose(106, 30);
    let outside =
        state.handle_raw_events(vec![RawInputEvent::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })]);
    assert!(outside.repaint);
    assert!(state.overlay.is_none());
}

#[test]
fn space_opens_a_conversation_preview_that_scrolls_and_closes() {
    let (mut state, open) = open_overlay();
    let request_id = endpoint_request(&open).id.clone();
    state.handle_endpoint_result("boot-1", &request_id, Ok(results("")));

    // In search mode space is text.
    state.handle_input_bytes(b"a b");
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay");
    };
    assert_eq!(overlay.query, "a b");
    assert!(overlay.preview.is_none());

    // Right arrow opens the preview even while the search line is focused.
    let arrow = state.handle_input_bytes(b"\x1b[C");
    assert!(matches!(
        &endpoint_request(&arrow).method,
        Method::AgentHistoryMessages(params) if params.session_id == "s-1"
    ));
    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"\x1b");
    let open_preview = state.handle_input_bytes(b" ");
    let request = endpoint_request(&open_preview);
    assert!(matches!(
        &request.method,
        Method::AgentHistoryMessages(params) if params.session_id == "s-1" && params.agent == "claude"
    ));
    let text = frame_text(&mut state);
    assert!(text.contains("← telegram bot deploy"), "{text}");
    assert!(text.contains("loading…"), "{text}");

    let messages_id = request.id.clone();
    let long = (1..=40)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (repaint, _) = state.handle_endpoint_result(
        "boot-1",
        &messages_id,
        Ok(ResponseResult::AgentHistoryMessages {
            conversation: crate::api::schema::AgentHistoryMessagesInfo {
                agent: "claude".into(),
                session_id: "s-1".into(),
                title: Some("telegram bot deploy".into()),
                project_path: "/Users/demo/alpha".into(),
                total: 2,
                offset: 0,
                truncated: false,
                messages: vec![
                    crate::agent_history::SessionMessage {
                        role: "user".into(),
                        text: "please add a telegram notification".into(),
                    },
                    crate::agent_history::SessionMessage {
                        role: "assistant".into(),
                        text: long,
                    },
                ],
            },
        }),
    );
    assert!(repaint);
    let text = frame_text(&mut state);
    assert!(text.contains(" YOU"), "{text}");
    assert!(
        text.contains("please add a telegram notification"),
        "{text}"
    );
    assert!(text.contains(" ASSISTANT"), "{text}");
    assert!(text.contains("2 messages"), "{text}");
    assert!(text.contains("lines 1–"), "{text}");
    assert!(state.hits.history_preview_max_scroll > 0);

    state.handle_input_bytes(b"j");
    state.handle_input_bytes(b"j");
    let Some(ClientShellOverlay::AgentHistory(ClientHistoryOverlay {
        preview: Some(preview),
        ..
    })) = state.overlay.as_ref()
    else {
        panic!("preview");
    };
    assert_eq!(preview.scroll, 2);
    let text = frame_text(&mut state);
    assert!(text.contains("lines 3–"), "{text}");
    state.handle_input_bytes(b"G");
    let text = frame_text(&mut state);
    assert!(text.contains("line 40"), "{text}");

    // Enter from the preview resumes the previewed session.
    let submit = state.handle_input_bytes(b"\r");
    let resume = endpoint_request(&submit);
    assert!(matches!(&resume.method, Method::AgentResume(params) if params.session_id == "s-1"));
    let resume_id = resume.id.clone();
    state.handle_endpoint_result(
        "boot-1",
        &resume_id,
        Err(ClientShellEndpointError {
            code: Some("project_missing".into()),
            message: "gone".into(),
        }),
    );

    // Esc closes the preview but keeps the list open; a second Esc closes the overlay.
    state.handle_input_bytes(b"\x1b");
    let Some(ClientShellOverlay::AgentHistory(overlay)) = state.overlay.as_ref() else {
        panic!("overlay should stay open");
    };
    assert!(overlay.preview.is_none());
    let text = frame_text(&mut state);
    assert!(text.contains("T telegram bot deploy"), "{text}");
    state.handle_input_bytes(b"\x1b");
    assert!(state.overlay.is_none());
}

#[test]
fn preview_result_for_another_session_is_ignored() {
    let (mut state, open) = open_overlay();
    let request_id = endpoint_request(&open).id.clone();
    state.handle_endpoint_result("boot-1", &request_id, Ok(results("")));
    state.handle_input_bytes(b"\x1b");
    let first = state.handle_input_bytes(b" ");
    let first_id = endpoint_request(&first).id.clone();
    state.handle_input_bytes(b"\x1b");
    state.handle_input_bytes(b"j");
    let second = state.handle_input_bytes(b" ");
    let second_request = endpoint_request(&second);
    assert!(
        matches!(&second_request.method, Method::AgentHistoryMessages(params) if params.session_id == "s-2")
    );
    let conversation = |id: &str| crate::api::schema::AgentHistoryMessagesInfo {
        agent: "claude".into(),
        session_id: id.into(),
        title: None,
        project_path: "/Users/demo/alpha".into(),
        total: 1,
        offset: 0,
        truncated: false,
        messages: vec![crate::agent_history::SessionMessage {
            role: "user".into(),
            text: format!("text of {id}"),
        }],
    };
    let (stale, _) = state.handle_endpoint_result(
        "boot-1",
        &first_id,
        Ok(ResponseResult::AgentHistoryMessages {
            conversation: conversation("s-1"),
        }),
    );
    assert!(!stale);
    let second_id = second_request.id.clone();
    let (applied, _) = state.handle_endpoint_result(
        "boot-1",
        &second_id,
        Ok(ResponseResult::AgentHistoryMessages {
            conversation: conversation("s-2"),
        }),
    );
    assert!(applied);
    let text = frame_text(&mut state);
    assert!(text.contains("text of s-2"), "{text}");
    assert!(!text.contains("text of s-1"), "{text}");
}
