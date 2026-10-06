use super::*;

#[tokio::test]
async fn headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client() {
    let mut server = test_headless_server();
    // Keep a shell reading its PTY so the resume command cannot race the
    // default test shell, which exits at once, before the input is queued.
    server
        .app
        .set_test_shell(shepr_test_support::fixture::idle_shell());
    let workspace = shepr_mux::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server
        .app
        .test_state_mut()
        .terminal_mut(pane_id)
        .plan_agent_resume(crate::test_support::test_codex_plan(
            "codex-session",
            vec![crate::app::exiting_test_command().into()],
        ));

    server.render_now();
    assert_eq!(
        server
            .app
            .state()
            .ws(0)
            .spawn_geometry()
            .map(|geometry| geometry.area),
        Some(server.app.state().settings().headless_rect())
    );

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now));
    assert!(server.app.test_runtimes_mut().get(&pane_id).is_none());
    let deadline = server
        .app
        .pending_agent_resume_wakeup()
        .expect("clientless resume should wait briefly for a host theme");

    assert!(server.handle_scheduled_tasks_headless(deadline));
    assert!(server.app.test_runtimes_mut().get(&pane_id).is_some());
    settle_resume_launch(&mut server, pane_id).await;
    shutdown_test_runtimes(&mut server);
}

/// Busy loop iterations (any pane printing keeps a render pending) must not
/// re-arm the theme wait: the first restored agent still launches once the
/// deadline armed on the first tick passes.
#[tokio::test]
async fn headless_scheduled_tasks_keep_pending_agent_resume_deadline_across_ticks() {
    let mut server = test_headless_server();
    // Keep a shell reading its PTY so the resume command cannot race the
    // default test shell, which exits at once, before the input is queued.
    server
        .app
        .set_test_shell(shepr_test_support::fixture::idle_shell());
    let workspace = shepr_mux::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tree().root();
    server
        .app
        .test_state_mut()
        .test_set_workspaces(vec![workspace]);
    server
        .app
        .test_state_mut()
        .terminal_mut(pane_id)
        .plan_agent_resume(crate::test_support::test_codex_plan(
            "codex-session",
            vec![crate::app::exiting_test_command().into()],
        ));
    server.render_now();

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now));
    let deadline = server
        .app
        .pending_agent_resume_wakeup()
        .expect("clientless resume should arm the theme wait");
    for step in 1..5 {
        let tick = now + Duration::from_millis(step * 100);
        assert!(
            tick < deadline,
            "test ticks must stay inside the theme wait"
        );
        assert!(!server.handle_scheduled_tasks_headless(tick));
        assert_eq!(server.app.pending_agent_resume_wakeup(), Some(deadline));
    }

    assert!(server.handle_scheduled_tasks_headless(deadline));
    assert!(server.app.test_runtimes_mut().get(&pane_id).is_some());
    shutdown_test_runtimes(&mut server);
}

/// Hands the app its queued runtime events, as the headless loop does, until
/// the resume's shell launch settled and typed its command (the plan is
/// consumed then, not at dispatch).
async fn settle_resume_launch(server: &mut HeadlessServer, pane_id: shepr_core::layout::PaneId) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while server
        .app
        .state()
        .terminal(pane_id)
        .expect("the pane is in the state")
        .agent_resume()
        .is_pending()
    {
        let event = tokio::time::timeout_at(deadline, server.outputs.next_event())
            .await
            .expect("the resume launch settles");
        server.app.handle_internal_event(event);
    }
}
