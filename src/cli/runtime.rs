use shepr_api::schema::{
    EmptyParams, Method, PaneFocusDirectionParams, PaneInputSetParams, PaneMoveParams,
    PaneRenameParams, PaneResizeParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneZoomParams, TabCreateParams, TabListParams, TabRenameParams, TabTarget,
    WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceRenameParams, WorkspaceTarget,
};

pub(super) fn workspace_list(paths: &super::target::CliContext) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:workspace:list",
        Method::WorkspaceList(EmptyParams::default()),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn workspace_create(
    paths: &super::target::CliContext,
    params: WorkspaceCreateParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:workspace:create",
        Method::WorkspaceCreate(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn workspace_get(
    paths: &super::target::CliContext,
    workspace_id: String,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:workspace:get",
        Method::WorkspaceGet(WorkspaceTarget { workspace_id }),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn workspace_focus(
    paths: &super::target::CliContext,
    workspace_id: String,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:workspace:focus",
        Method::WorkspaceFocus(WorkspaceTarget { workspace_id }),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn workspace_rename(
    paths: &super::target::CliContext,
    params: WorkspaceRenameParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:workspace:rename",
        Method::WorkspaceRename(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn workspace_close(
    paths: &super::target::CliContext,
    params: WorkspaceCloseParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:workspace:close",
        Method::WorkspaceClose(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn tab_list(
    paths: &super::target::CliContext,
    params: TabListParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:tab:list",
        Method::TabList(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn tab_create(
    paths: &super::target::CliContext,
    params: TabCreateParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:tab:create",
        Method::TabCreate(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn tab_get(paths: &super::target::CliContext, tab_id: String) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:tab:get",
        Method::TabGet(TabTarget { tab_id }),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn tab_focus(
    paths: &super::target::CliContext,
    tab_id: String,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:tab:focus",
        Method::TabFocus(TabTarget { tab_id }),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn tab_rename(
    paths: &super::target::CliContext,
    params: TabRenameParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:tab:rename",
        Method::TabRename(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn tab_close(
    paths: &super::target::CliContext,
    tab_id: String,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:tab:close",
        Method::TabClose(TabTarget { tab_id }),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_focus(
    paths: &super::target::CliContext,
    params: PaneFocusDirectionParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:focus",
        Method::PaneFocusDirection(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_resize(
    paths: &super::target::CliContext,
    params: PaneResizeParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:resize",
        Method::PaneResize(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_zoom(
    paths: &super::target::CliContext,
    params: PaneZoomParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:zoom",
        Method::PaneZoom(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_rename(
    paths: &super::target::CliContext,
    params: PaneRenameParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:rename",
        Method::PaneRename(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_input_set(
    paths: &super::target::CliContext,
    params: PaneInputSetParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:input:set",
        Method::PaneInputSet(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_split(
    paths: &super::target::CliContext,
    params: PaneSplitParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:split",
        Method::PaneSplit(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_swap(
    paths: &super::target::CliContext,
    params: PaneSwapParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:swap",
        Method::PaneSwap(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_move(
    paths: &super::target::CliContext,
    params: PaneMoveParams,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:move",
        Method::PaneMove(params),
        super::MethodResponseMode::Print,
    )
}

pub(super) fn pane_close(
    paths: &super::target::CliContext,
    pane_id: String,
) -> super::CliResult<i32> {
    super::send_method_response(
        paths,
        "cli:pane:close",
        Method::PaneClose(PaneTarget { pane_id }),
        super::MethodResponseMode::Print,
    )
}
