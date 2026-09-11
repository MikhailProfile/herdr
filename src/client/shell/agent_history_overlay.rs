use super::*;

pub(super) fn render_agent_history_overlay(
    b: &mut Buffer,
    h: &ClientHistoryOverlay,
    p: &Palette,
) -> Option<OverlayRender> {
    let a = b.area;
    let mx = (a.width / 16).max(2);
    let my = (a.height / 10).max(1);
    let q = Rect::new(
        a.x + mx,
        a.y + my,
        a.width.saturating_sub(mx * 2).max(4),
        a.height.saturating_sub(my * 2).max(4),
    );
    let i = panel(b, q, p.accent, p.panel_bg)?;
    if let Some(preview) = h.preview.as_ref() {
        return Some(render_preview(b, q, i, h, preview, p));
    }
    let rows = super::super::super::agent_history::history_rows(h);

    let search = if h.query.is_empty() && !h.search_focused {
        " / search past sessions".to_owned()
    } else {
        format!(" / {}", h.query)
    };
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &search,
        Style::default()
            .fg(if h.search_focused { p.text } else { p.overlay0 })
            .bg(p.panel_bg),
    );
    let sessions: usize = h.groups.iter().map(|group| group.sessions.len()).sum();
    let summary = if h.searching {
        "searching…".to_owned()
    } else {
        format!(
            "{sessions} session{} · {} project{}",
            if sessions == 1 { "" } else { "s" },
            h.groups.len(),
            if h.groups.len() == 1 { "" } else { "s" }
        )
    };
    put_right_text(
        b,
        i,
        i.y,
        &summary,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &"─".repeat(i.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );

    let body = Rect::new(i.x, i.y + 2, i.width, i.height.saturating_sub(4));
    let mut row_hits = Vec::new();
    if rows.is_empty() {
        let empty = if h.searching {
            " searching…"
        } else if h.query.is_empty() {
            " no indexed sessions yet · press r to scan transcripts"
        } else {
            " no sessions match"
        };
        put_text(
            b,
            body.x,
            body.y,
            body.width,
            empty,
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    let selected =
        super::super::super::agent_history::history_selected_index(&rows, h).unwrap_or(0);
    let max = rows.len().saturating_sub(body.height as usize);
    let scroll = h
        .scroll
        .max(selected.saturating_sub(body.height.saturating_sub(1) as usize))
        .min(selected)
        .min(max);
    for (ix, r) in rows
        .iter()
        .enumerate()
        .skip(scroll)
        .take(body.height as usize)
    {
        let rect = Rect::new(body.x, body.y + (ix - scroll) as u16, body.width, 1);
        row_hits.push((rect, r.target.clone()));
        let st = if ix == selected {
            Style::default()
                .fg(contrast(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(if r.depth == 0 { p.text } else { p.subtext0 })
                .bg(p.panel_bg)
        };
        b.set_style(rect, st);
        let open = if r.open { "◆ " } else { "" };
        let label = if r.depth == 0 {
            let caret = if r.collapsed { "▸" } else { "▾" };
            format!(" {caret} {open}{}", r.label)
        } else {
            let next_is_sibling = rows.get(ix + 1).is_some_and(|next| next.depth == r.depth);
            let branch = if next_is_sibling {
                "├──"
            } else {
                "└──"
            };
            format!("   {branch} {} {open}{}", r.badge, r.label)
        };
        put_text(b, rect.x, rect.y, rect.width, &label, st);
        if !r.meta.is_empty() {
            let label_width = display_width(&label).min(rect.width);
            let meta = Rect::new(
                rect.x.saturating_add(label_width).saturating_add(1),
                rect.y,
                rect.width.saturating_sub(label_width.saturating_add(1)),
                1,
            );
            let meta_style = if ix == selected {
                st
            } else {
                Style::default().fg(p.overlay0).bg(p.panel_bg)
            };
            put_right_text(b, meta, rect.y, &r.meta, meta_style);
        }
    }

    let dy = i.bottom() - 2;
    if let Some(error) = h.error.as_deref() {
        put_text(
            b,
            i.x,
            dy,
            i.width,
            &format!(" {error}"),
            Style::default().fg(p.red).bg(p.panel_bg),
        );
    } else if h.resuming {
        put_text(
            b,
            i.x,
            dy,
            i.width,
            " resuming…",
            Style::default().fg(p.accent).bg(p.panel_bg),
        );
    } else if let Some(r) = rows.get(selected) {
        put_text(
            b,
            i.x,
            dy,
            i.width,
            &format!(" {}", r.detail),
            Style::default().fg(p.overlay0).bg(p.panel_bg),
        );
    }
    put_text(
        b,
        i.x,
        i.bottom() - 1,
        i.width,
        if h.search_focused {
            " search type · move ↑↓/ctrl+n/p · preview → · resume enter · back esc"
        } else {
            " move j/k · preview space/→ · resume enter · new workspace w · rescan r · search / · close esc"
        },
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    Some(OverlayRender {
        history_popup: q,
        history_search: Rect::new(i.x, i.y, i.width, 1),
        history_rows: row_hits,
        cursor: h.search_focused.then(|| crate::protocol::CursorState {
            x: i.x + 3 + display_width(&h.query),
            y: i.y,
            visible: true,
            shape: 0,
        }),
        ..OverlayRender::default()
    })
}

enum PreviewLine<'a> {
    Role(&'a str),
    Text(String),
    Blank,
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0usize;
        for word in raw.split(' ') {
            let word_width = usize::from(display_width(word));
            let fits_appended = !current.is_empty() && current_width + 1 + word_width <= width;
            if fits_appended {
                current.push(' ');
                current.push_str(word);
                current_width += 1 + word_width;
                continue;
            }
            if !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
                continue;
            }
            for character in word.chars() {
                let character_width = character.width().unwrap_or(0);
                if current_width + character_width > width && !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                current.push(character);
                current_width += character_width;
            }
        }
        lines.push(current);
    }
    lines
}

fn render_preview(
    b: &mut Buffer,
    q: Rect,
    i: Rect,
    h: &ClientHistoryOverlay,
    preview: &ClientHistoryPreview,
    p: &Palette,
) -> OverlayRender {
    let header = format!(" ← {}", preview.title);
    put_text(
        b,
        i.x,
        i.y,
        i.width,
        &header,
        Style::default()
            .fg(p.text)
            .bg(p.panel_bg)
            .add_modifier(Modifier::BOLD),
    );
    let summary = if preview.loading {
        "loading…".to_owned()
    } else {
        format!(
            "{} message{}{}",
            preview.total,
            if preview.total == 1 { "" } else { "s" },
            if preview.truncated {
                " · truncated"
            } else {
                ""
            }
        )
    };
    put_right_text(
        b,
        i,
        i.y,
        &summary,
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    put_text(
        b,
        i.x,
        i.y + 1,
        i.width,
        &"─".repeat(i.width as usize),
        Style::default().fg(p.surface1).bg(p.panel_bg),
    );

    let body = Rect::new(i.x, i.y + 2, i.width, i.height.saturating_sub(4));
    let text_width = usize::from(body.width.saturating_sub(3)).max(1);
    let mut lines: Vec<PreviewLine> = Vec::new();
    for message in &preview.messages {
        lines.push(PreviewLine::Role(match message.role.as_str() {
            "user" => "YOU",
            "assistant" => "ASSISTANT",
            other => other,
        }));
        lines.extend(
            wrap_text(&message.text, text_width)
                .into_iter()
                .map(PreviewLine::Text),
        );
        lines.push(PreviewLine::Blank);
    }
    if lines.is_empty() && !preview.loading {
        lines.push(PreviewLine::Text(
            "no user or assistant text in this session".into(),
        ));
    }
    let max_scroll = lines.len().saturating_sub(body.height as usize);
    let scroll = preview.scroll.min(max_scroll);
    for (offset, line) in lines
        .iter()
        .skip(scroll)
        .take(body.height as usize)
        .enumerate()
    {
        let y = body.y + offset as u16;
        match line {
            PreviewLine::Role(role) => put_text(
                b,
                body.x,
                y,
                body.width,
                &format!(" {role}"),
                Style::default()
                    .fg(if *role == "YOU" { p.accent } else { p.green })
                    .bg(p.panel_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            PreviewLine::Text(text) => put_text(
                b,
                body.x,
                y,
                body.width,
                &format!("   {text}"),
                Style::default().fg(p.text).bg(p.panel_bg),
            ),
            PreviewLine::Blank => {}
        }
    }

    let dy = i.bottom() - 2;
    let status = if let Some(error) = h.error.as_deref() {
        (format!(" {error}"), p.red)
    } else if h.resuming {
        (" resuming…".to_owned(), p.accent)
    } else if lines.is_empty() {
        (String::new(), p.overlay0)
    } else {
        let shown_end = (scroll + body.height as usize).min(lines.len());
        (
            format!(" lines {}–{} of {}", scroll + 1, shown_end, lines.len()),
            p.overlay0,
        )
    };
    put_text(
        b,
        i.x,
        dy,
        i.width,
        &status.0,
        Style::default().fg(status.1).bg(p.panel_bg),
    );
    put_text(
        b,
        i.x,
        i.bottom() - 1,
        i.width,
        " scroll j/k · page ctrl+d/u · top g · end G · resume enter · new workspace w · back esc",
        Style::default().fg(p.overlay0).bg(p.panel_bg),
    );
    OverlayRender {
        history_popup: q,
        history_search: Rect::default(),
        history_rows: Vec::new(),
        history_preview_max_scroll: max_scroll,
        ..OverlayRender::default()
    }
}

#[cfg(test)]
mod tests {
    use super::wrap_text;

    #[test]
    fn wraps_words_hard_splits_long_tokens_and_keeps_blank_lines() {
        assert_eq!(wrap_text("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap_text("широкий текст", 8), vec!["широкий", "текст"]);
        assert_eq!(wrap_text("", 5), vec![""]);
    }
}
