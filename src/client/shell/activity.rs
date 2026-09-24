//! Pure display of optional endpoint activity. No pane targets or action hits.
use super::*;
use crate::api::schema::ActivitySource;

pub(super) fn render_strip<'a>(
    buffer: &mut Buffer,
    area: Rect,
    sources: impl Iterator<Item = (&'a str, &'a [ActivitySource], bool)>,
    config: &ClientShellConfig,
) -> Rect {
    if area.height == 0 || area.width == 0 {
        return area;
    }
    let sources: Vec<_> = sources.collect();
    let total: usize = sources
        .iter()
        .flat_map(|(_, sources, _)| *sources)
        .map(|source| source.tasks.len() + source.omitted as usize)
        .sum();
    if total == 0 {
        return area;
    }
    let height = area.height.div_ceil(2).min(11);
    let strip = Rect::new(area.x, area.bottom() - height, area.width, height);
    let remaining = Rect::new(area.x, area.y, area.width, area.height - height);
    let capacity = usize::from(height.saturating_sub(2) / 4);
    let dim = Style::default().fg(config.palette.overlay0);
    super::render::put_text(
        buffer,
        strip.x,
        strip.y,
        strip.width,
        &format!(" headless · ephemeral ({total})"),
        dim.add_modifier(Modifier::BOLD),
    );
    let mut drawn = 0;
    for (machine, sources, stale) in sources {
        for task in sources.iter().flat_map(|source| &source.tasks) {
            if drawn >= capacity {
                break;
            }
            let y = strip.y + 1 + drawn as u16 * 4;
            let scope = if machine.is_empty() {
                String::new()
            } else {
                format!("{machine}/")
            };
            let elapsed = task
                .elapsed_seconds
                .map(|s| format!("{s}s"))
                .unwrap_or_else(|| "?s".into());
            let lines = [
                format!(" {scope}{}", task.request_id),
                format!(
                    " {}/{}",
                    task.model.as_deref().unwrap_or("default?"),
                    task.effort.as_deref().unwrap_or("default?")
                ),
                format!(" {} · {elapsed}", task.workspace),
                if stale {
                    " STALE · last observation".into()
                } else {
                    format!(" {} · {}", task.status, task.progress)
                },
            ];
            for (i, line) in lines.iter().enumerate() {
                super::render::put_text(
                    buffer,
                    strip.x,
                    y + i as u16,
                    strip.width,
                    line,
                    if stale {
                        dim
                    } else {
                        Style::default().fg(config.palette.subtext0)
                    },
                );
            }
            drawn += 1;
        }
    }
    if total > drawn && height > 1 {
        super::render::put_text(
            buffer,
            strip.x,
            strip.bottom() - 1,
            strip.width,
            &format!(" +{} more · api snapshot", total - drawn),
            dim,
        );
    }
    remaining
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_strip_has_no_native_actions_and_preserves_agents() {
        let config = ClientShellConfig::from_config(&crate::config::Config::default());
        let report = crate::app::activity::tests::report();
        let source = ActivitySource {
            source: report.source,
            tasks: report.tasks,
            omitted: 3,
        };
        let mut snapshot = super::super::tests::snapshot();
        snapshot.activity = vec![source];
        snapshot.agents.push(crate::protocol::ClientShellAgent {
            pane_id: "pane_1".into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            name: Some("director".into()),
            display_agent: None,
            agent: Some("codex".into()),
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            agent_status: crate::api::schema::AgentStatus::Idle,
            state_change_seq: 1,
            state_labels: Vec::new(),
            tokens: Vec::new(),
            focused: true,
        });
        snapshot.agents[0]
            .tokens
            .push(("hygiene_kind".into(), "durable".into()));
        let area = Rect::new(0, 0, 78, 24);
        let mut buffer = Buffer::empty(area);
        let mut hits = ShellHitMap::default();
        super::super::agent_sidebar::render_agent_panel(
            &mut buffer,
            area,
            &snapshot,
            &config,
            &mut 0,
            &mut hits,
        );
        let text = buffer
            .content
            .chunks(78)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("headless · ephemeral (4)"));
        assert!(text.contains("build-a"));
        assert!(text.contains("sol/high"));
        assert!(text.contains("canvas · 42s"));
        assert!(text.contains("running · monitoring"));
        assert!(text.contains("+3 more"));
        assert!(text.contains("durable"));
        assert!(hits.agents.iter().all(|(rect, _)| rect.bottom() <= 13));
        if let Ok(path) = std::env::var("HERDR_ACTIVITY_RENDER_ARTIFACT") {
            std::fs::write(path, &text).unwrap();
        }
        // All geometry, including collapsed/very short terminals, stays bounded.
        for width in [0, 1, 4, 24] {
            for height in [0, 1, 2, 3, 5] {
                let area = Rect::new(0, 0, width, height);
                let mut buffer = Buffer::empty(area);
                let rest = render_strip(
                    &mut buffer,
                    area,
                    std::iter::once(("remote", snapshot.activity.as_slice(), true)),
                    &config,
                );
                assert!(rest.bottom() <= area.bottom());
            }
        }
        let mut buffer = Buffer::empty(area);
        render_strip(
            &mut buffer,
            area,
            std::iter::once(("remote", snapshot.activity.as_slice(), true)),
            &config,
        );
        let text = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.contains("remote/build-a"));
        assert!(text.contains("STALE"));
        assert!(!text.contains("running · monitoring"));
    }
}
