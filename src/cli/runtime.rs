use crate::api::schema::{
    EmptyParams, Method, PaneFocusDirectionParams, PaneInputSetParams, PaneMoveParams,
    PaneRenameParams, PaneResizeParams, PaneSplitParams, PaneSwapParams, PaneTarget,
    PaneZoomParams, Request, TabCreateParams, TabListParams, TabRenameParams, TabTarget,
    WorkspaceCloseParams, WorkspaceCreateParams, WorkspaceRenameParams, WorkspaceTarget,
};

fn print_method_response(
    paths: &super::target::CliContext,
    id: &'static str,
    method: Method,
) -> std::io::Result<i32> {
    super::print_response(&super::send_request(
        paths,
        &Request {
            id: id.into(),
            method,
        },
    )?)
}

pub(super) fn workspace_list(paths: &super::target::CliContext) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:workspace:list",
        Method::WorkspaceList(EmptyParams::default()),
    )
}

pub(super) fn workspace_create(
    paths: &super::target::CliContext,
    params: WorkspaceCreateParams,
) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:workspace:create",
        Method::WorkspaceCreate(params),
    )
}

pub(super) fn workspace_get(
    paths: &super::target::CliContext,
    workspace_id: String,
) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:workspace:get",
        Method::WorkspaceGet(WorkspaceTarget { workspace_id }),
    )
}

pub(super) fn workspace_focus(
    paths: &super::target::CliContext,
    workspace_id: String,
) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:workspace:focus",
        Method::WorkspaceFocus(WorkspaceTarget { workspace_id }),
    )
}

pub(super) fn workspace_rename(
    paths: &super::target::CliContext,
    params: WorkspaceRenameParams,
) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:workspace:rename",
        Method::WorkspaceRename(params),
    )
}

pub(super) fn workspace_close(
    paths: &super::target::CliContext,
    params: WorkspaceCloseParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:workspace:close", Method::WorkspaceClose(params))
}

pub(super) fn tab_list(
    paths: &super::target::CliContext,
    params: TabListParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:tab:list", Method::TabList(params))
}

pub(super) fn tab_create(
    paths: &super::target::CliContext,
    params: TabCreateParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:tab:create", Method::TabCreate(params))
}

pub(super) fn tab_get(paths: &super::target::CliContext, tab_id: String) -> std::io::Result<i32> {
    print_method_response(paths, "cli:tab:get", Method::TabGet(TabTarget { tab_id }))
}

pub(super) fn tab_focus(paths: &super::target::CliContext, tab_id: String) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:tab:focus",
        Method::TabFocus(TabTarget { tab_id }),
    )
}

pub(super) fn tab_rename(
    paths: &super::target::CliContext,
    params: TabRenameParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:tab:rename", Method::TabRename(params))
}

pub(super) fn tab_close(paths: &super::target::CliContext, tab_id: String) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:tab:close",
        Method::TabClose(TabTarget { tab_id }),
    )
}

pub(super) fn pane_focus(
    paths: &super::target::CliContext,
    params: PaneFocusDirectionParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:focus", Method::PaneFocusDirection(params))
}

pub(super) fn pane_resize(
    paths: &super::target::CliContext,
    params: PaneResizeParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:resize", Method::PaneResize(params))
}

pub(super) fn pane_zoom(
    paths: &super::target::CliContext,
    params: PaneZoomParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:zoom", Method::PaneZoom(params))
}

pub(super) fn pane_rename(
    paths: &super::target::CliContext,
    params: PaneRenameParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:rename", Method::PaneRename(params))
}

pub(super) fn pane_input_set(
    paths: &super::target::CliContext,
    params: PaneInputSetParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:input:set", Method::PaneInputSet(params))
}

pub(super) fn pane_split(
    paths: &super::target::CliContext,
    params: PaneSplitParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:split", Method::PaneSplit(params))
}

pub(super) fn pane_swap(
    paths: &super::target::CliContext,
    params: PaneSwapParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:swap", Method::PaneSwap(params))
}

pub(super) fn pane_move(
    paths: &super::target::CliContext,
    params: PaneMoveParams,
) -> std::io::Result<i32> {
    print_method_response(paths, "cli:pane:move", Method::PaneMove(params))
}

pub(super) fn pane_close(
    paths: &super::target::CliContext,
    pane_id: String,
) -> std::io::Result<i32> {
    print_method_response(
        paths,
        "cli:pane:close",
        Method::PaneClose(PaneTarget { pane_id }),
    )
}
