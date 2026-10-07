use crate::limits::API_REQUEST_DRAIN_LIMIT;

impl super::HeadlessServer {
    pub(super) fn handle_api_request_with_shutdown_check(
        &mut self,
        msg: shepr_api::ApiRequestMessage,
    ) -> bool {
        if !self.begin_request_dispatch() {
            self.reject_api_request_for_shutdown(&msg);
            return false;
        }
        // No socket method changes topology, moves focus or changes geometry.
        // The topology changes this path can still reach settle themselves:
        // a pane death drained before the request runs
        // (`handle_admitted_pane_death`) and the automatic workspace created
        // after it (`create_automatic_workspace`) each reconcile client
        // locations, reapply geometry and sync pane focus.
        self.dispatch_api_request(msg)
    }

    pub(super) fn drain_api_requests_with_shutdown_check(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..API_REQUEST_DRAIN_LIMIT {
            // Recheck before each dequeue so a stop during this batch leaves
            // later requests for shutdown refusal.
            if self.lifecycle.stop_requested() {
                break;
            }
            let Ok(msg) = self.api_request_rx.try_recv() else {
                break;
            };
            changed |= self.handle_api_request_with_shutdown_check(msg);
        }
        changed
    }

    pub(super) fn reject_queued_api_requests_for_shutdown(&mut self) {
        self.api_request_rx.close();
        // Closing first makes this exhaustive cleanup finite: every request
        // already accepted gets a refusal, and no new request can extend it.
        while let Ok(msg) = self.api_request_rx.try_recv() {
            self.reject_api_request_for_shutdown(&msg);
        }
    }

    pub(super) fn reject_api_request_for_shutdown(&self, msg: &shepr_api::ApiRequestMessage) {
        let error = self.lifecycle.shutdown_error();
        let request_id = msg.request.id.clone();
        let method = msg.request.method.traits().name;
        let response = Err(error);
        shepr_api::send_api_response(&msg.respond_to, &request_id, method, response);
    }
}
