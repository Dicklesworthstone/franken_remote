//! Approved metadata-only inspection on the original connection. No display
//! selection, media attachment, native worker or input grant is created.
use super::{
    ApprovalNotice, Attempt, Budget, Catalog, Error, Future, Policy, Viewer, ViewerSession,
    approved, retain,
};

impl Viewer {
    /// Inspect the host's bounded display catalog after normal negotiation and
    /// optional local approval, then close this one session. The result is a
    /// snapshot, NOT authority or a stable identity for a later connection.
    /// Reconnecting must obtain and validate the new session's catalog again.
    ///
    /// Approval is a bounded notification callback, never a remote approval RPC.
    /// The original call-time budget includes unpolled time and approval. Failure,
    /// cancellation, callback unwind and unpolled abandonment fence this viewer.
    pub fn inspect_displays<'a>(
        self,
        policy: Policy,
        mut approval: impl FnMut(ApprovalNotice) -> Result<(), ()> + 'a,
    ) -> impl Future<Output = Result<Catalog, Error>> + 'a {
        let cx = self.cx.clone();
        let budget = Budget::new(cx.clone(), policy);
        Attempt {
            cx,
            complete: false,
            inner: Box::pin(async move {
                let budget = budget?;
                let session = Box::pin(approved(self, &budget, &mut approval)).await?;
                Box::pin(inspect(session, &budget)).await
            }),
        }
    }
}
impl ViewerSession {
    /// Metadata-only inspection of this already-approved session. Consumes and
    /// closes it even on success; the returned catalog cannot authorize media or
    /// input, and no unfinished display exchange survives to a later attempt.
    pub fn inspect_displays(self, policy: Policy) -> impl Future<Output = Result<Catalog, Error>> {
        let cx = self.cx.clone();
        let budget = Budget::new(cx.clone(), policy);
        Attempt {
            cx,
            complete: false,
            inner: Box::pin(async move { Box::pin(inspect(self, &budget?)).await }),
        }
    }
}
async fn inspect(mut session: ViewerSession, budget: &Budget) -> Result<Catalog, Error> {
    let mut selection = session
        .select_display(budget.remaining()?)
        .map_err(Error::Display)?;
    loop {
        budget.remaining()?;
        let (q, _) = session.io().map_err(Error::Session)?;
        selection.dispatch(q).map_err(Error::Display)?;
        if let Some(catalog) = selection.catalog(q).map_err(Error::Display)?.copied() {
            // Copy only the fixed-size, validated metadata. Never choose or
            // transmit SelectDisplay, even when the catalog contains one entry.
            budget.remaining()?;
            session.close();
            return Ok(catalog);
        }
        session
            .drive(budget.wait()?, retain)
            .await
            .map_err(Error::Session)?;
    }
}
