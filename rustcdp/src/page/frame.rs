use super::{Element, EvaluationBuilder, FunctionCallBuilder, Locator, Page};
use crate::{
    Error, Result,
    cdp::{
        dom::commands::GetFrameOwner,
        page::{Frame as FrameInfo, FrameId, FrameTree, commands::GetFrameTree},
        target::{TargetId, commands::GetTargetInfo},
    },
    error::with_cleanup,
};
use std::time::Duration;

/// A frame within a CDP target. Clones share the page's resource ownership.
/// Each evaluation resolves the frame's current unique execution context.
/// A removed frame or a process swap invalidates the handle; acquire a new one.
#[derive(Clone)]
pub struct Frame {
    page: Page,
    id: FrameId,
}

fn find<'a>(tree: &'a FrameTree, id: &FrameId) -> Option<&'a FrameTree> {
    if tree.frame.id == *id {
        return Some(tree);
    }
    tree.child_frames
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find_map(|child| find(child, id))
}

impl Page {
    /// Attach to the specified out-of-process iframe target belonging to this page.
    /// Discover candidate target IDs with typed `Target.getTargets`. Target IDs are
    /// distinct from FrameIds: attachment reads the child's actual root FrameId
    /// and validates its owner through this page's session. No target is retried
    /// or substituted. Same-target frames use [`Self::main_frame`], [`Self::frames`]
    /// or [`Self::frame_by_id`].
    /// Ownership is a snapshot, not an atomic promise across navigation/process
    /// swaps. Deeper remote descendants may require their containing target.
    ///
    /// An owned task retains the parent through attachment, validation and setup.
    /// The handle timeout limits caller waiting, not that task. Target inspection
    /// and ownership validation each have a 30-second bound; attachment and domain
    /// setup retain their independent bounds. Failed adoption awaits detachment;
    /// failed detachment closes the connection to resolve ownership. No target
    /// close command is sent. Unclaimed success uses normal attachment Drop cleanup.
    ///
    /// The returned iframe Page retains any parent fallback-context lease and
    /// restores the handle timeout. Separate child sessions do not automatically
    /// receive proxy authentication workers. This does not enable auto-attach or
    /// follow process swaps; callers must explicitly acquire a new attachment.
    pub async fn attach_frame_target(&self, target: &TargetId) -> Result<Page> {
        if target == self.session.target_id() {
            return Err(Error::InvalidArgument(
                "cannot attach this page as its own iframe".into(),
            ));
        }
        let parent = self.clone();
        let target = target.clone();
        let connection = self.session.connection();
        let timeout = connection.command_timeout();
        let task = connection.runtime().spawn(async move {
            let connection = parent
                .session
                .connection()
                .clone()
                .with_command_timeout(None);
            let response = tokio::time::timeout(
                Duration::from_secs(30),
                connection.execute(&GetTargetInfo::default().with_target_id(target.clone())),
            )
            .await
            .map_err(|_| Error::Timeout("inspecting iframe target".into()))??;
            if response.target_info.target_id != target {
                return Err(Error::UnexpectedResponse {
                    method: "Target.getTargetInfo".into(),
                    payload: serde_json::to_value(response)?,
                });
            }
            if response.target_info.type_ != "iframe" {
                return Err(Error::InvalidArgument("target is not an iframe".into()));
            }
            // Await attachment's own bounded acquisition/recovery, without an
            // outer timeout that could abandon a still-acquiring nested owner.
            let session = connection.attach(target).await?;
            let adopted = async {
                tokio::time::timeout(Duration::from_secs(30), async {
                    let response = session.execute(&GetFrameTree::default()).await?;
                    if response.frame_tree.frame.id.0.is_empty() {
                        return Err(Error::UnexpectedResponse {
                            method: "Page.getFrameTree".into(),
                            payload: serde_json::to_value(response)?,
                        });
                    }
                    parent
                        .session
                        .clone()
                        .with_command_timeout(None)
                        .execute(&GetFrameOwner::new(response.frame_tree.frame.id))
                        .await?;
                    Ok::<(), Error>(())
                })
                .await
                .map_err(|_| Error::Timeout("validating iframe ownership".into()))??;
                // Domain acquisitions own their bounded transitions and recovery.
                // Do not interrupt them with a competing setup timeout/detach.
                let mut page = Page::from_session(
                    session.clone(),
                    parent.context.clone(),
                    super::TargetKind::Iframe,
                )
                .await?;
                page.context_lease = parent.context_lease.clone();
                Ok(page.with_command_timeout(timeout))
            }
            .await;
            let result = match adopted {
                Ok(page) => Ok(page),
                Err(error) => {
                    let cleanup = session.detach().await;
                    if cleanup.is_err() && !session.is_closed() {
                        connection.close().await;
                    }
                    with_cleanup(Err(error), cleanup)
                }
            };
            // Keep the parent and its fallback lease alive through all recovery.
            drop(parent);
            result
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("attaching iframe target".into()))??
        } else {
            task.await?
        }
    }

    pub async fn main_frame(&self) -> Result<Frame> {
        let tree = self.session.execute(&GetFrameTree::new()).await?.frame_tree;
        Ok(Frame {
            page: self.clone(),
            id: tree.frame.id,
        })
    }

    /// Find an ID in one snapshot of this target's frame tree, including its root.
    /// Returns `None` if absent; protocol and decoding errors propagate.
    /// This does not attach out-of-process targets, poll, or retry.
    /// The ID identifies a frame, not a particular document or a durable locator:
    /// navigation can replace its document, and removal or a process swap can
    /// invalidate the returned handle immediately after this lookup.
    pub async fn frame_by_id(&self, id: &FrameId) -> Result<Option<Frame>> {
        let tree = self.session.execute(&GetFrameTree::new()).await?.frame_tree;
        Ok(find(&tree, id).map(|tree| Frame {
            page: self.clone(),
            id: tree.frame.id.clone(),
        }))
    }

    /// Snapshot all frames exposed by this target's frame tree, including its root.
    /// OOPIF targets are separate: use [`Self::attach_frame_target`], then
    /// [`Self::main_frame`].
    pub async fn frames(&self) -> Result<Vec<Frame>> {
        let tree = self.session.execute(&GetFrameTree::new()).await?.frame_tree;
        let mut stack = vec![tree];
        let mut frames = Vec::new();
        while let Some(tree) = stack.pop() {
            frames.push(Frame {
                page: self.clone(),
                id: tree.frame.id,
            });
            if let Some(children) = tree.child_frames {
                stack.extend(children.into_iter().rev());
            }
        }
        Ok(frames)
    }
}

impl Frame {
    pub(super) fn page(&self) -> &Page {
        &self.page
    }

    pub fn id(&self) -> &FrameId {
        &self.id
    }

    pub async fn info(&self) -> Result<FrameInfo> {
        let tree = self
            .page
            .session
            .execute(&GetFrameTree::new())
            .await?
            .frame_tree;
        find(&tree, &self.id)
            .map(|tree| tree.frame.clone())
            .ok_or_else(|| Error::FrameNotFound(self.id.clone()))
    }

    /// Snapshot direct children belonging to this target's frame tree.
    pub async fn children(&self) -> Result<Vec<Frame>> {
        let tree = self
            .page
            .session
            .execute(&GetFrameTree::new())
            .await?
            .frame_tree;
        let tree = find(&tree, &self.id).ok_or_else(|| Error::FrameNotFound(self.id.clone()))?;
        Ok(tree
            .child_frames
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|child| Frame {
                page: self.page.clone(),
                id: child.frame.id.clone(),
            })
            .collect())
    }

    /// Configure evaluation in this frame's main world.
    /// See [`Page::evaluate`] for ownership and cancellation guarantees.
    pub fn evaluate(&self, expression: impl Into<String>) -> EvaluationBuilder<'_> {
        EvaluationBuilder::for_frame(&self.page, self.id.clone(), expression)
    }

    /// Configure a function call with JSON arguments in this frame's main world.
    /// Each call resolves the frame's current unique execution context, without
    /// retrying user source. See [`FunctionCallBuilder`] for result, ownership,
    /// and cancellation guarantees.
    pub fn call(&self, function: impl Into<String>) -> FunctionCallBuilder<'_> {
        FunctionCallBuilder::for_frame(&self.page, self.id.clone(), function)
    }

    /// Create an owned CSS locator scoped to this frame's current document.
    /// Construction performs no protocol work. The locator retains this frame's
    /// identity across document navigation, but does not adopt a removed frame or
    /// follow a process swap. See [`Locator::wait_for`] for state and timeout rules.
    pub fn locator(&self, selector: impl Into<String>) -> Locator {
        Locator::for_frame(self.clone(), selector)
    }

    /// Serialize this frame's document, including its doctype when present.
    /// Uses the same ownership and cleanup policy as [`Page::html`].
    pub async fn html(&self) -> Result<String> {
        super::dom::document_html(&self.page.session, self.evaluate("document")).await
    }

    /// Find a node in this frame with the owned acquisition and cleanup policy
    /// of [`Page::element_by_selector`].
    pub async fn element_by_selector(&self, selector: &str) -> Result<Option<Element>> {
        let selector = serde_json::to_string(selector)?;
        self.element_for_expression(format!("document.querySelector({selector})"))
            .await
    }

    /// Find the first CSS match in this frame without polling, or return
    /// [`Error::ElementNotFound`] if absent. Other errors propagate unchanged.
    /// Uses the owned acquisition and cancellation policy of [`Self::element_by_selector`].
    pub async fn require_element(&self, selector: &str) -> Result<Element> {
        self.element_by_selector(selector)
            .await?
            .ok_or_else(|| Error::ElementNotFound(selector.to_owned()))
    }

    /// Wait for the first present CSS match in this frame, as in [`Page::wait_for_element`].
    /// Polls immediately, then every 100 milliseconds after unsuccessful acquisition;
    /// only recognized context changes are retried. The timeout includes acquisition
    /// and polling; shorter command timeouts still apply. No visibility, stability,
    /// or actionability is promised, and the returned element can become stale.
    /// Cancellation or timeout leaves in-flight owned acquisition running under its
    /// independent 30-second limit and recovery/cleanup bounds. This does not
    /// reattach a removed frame or follow a process swap.
    pub async fn wait_for_element(&self, selector: &str, timeout: Duration) -> Result<Element> {
        let expression = format!(
            "document.querySelector({})",
            serde_json::to_string(selector)?
        );
        self.page
            .wait_for_expression_in_frame(expression, selector, timeout, Some(self.id.clone()))
            .await
    }

    /// Find an XPath node in this frame with the owned acquisition and cleanup
    /// policy of [`Page::element_by_xpath`].
    pub async fn element_by_xpath(&self, xpath: &str) -> Result<Option<Element>> {
        let xpath = serde_json::to_string(xpath)?;
        self.element_for_expression(format!("document.evaluate({xpath}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue")).await
    }

    /// Find the first XPath node in this frame without polling, or return
    /// [`Error::ElementNotFound`] if absent. Other errors propagate unchanged.
    /// Uses the owned acquisition and cancellation policy of [`Self::element_by_xpath`].
    pub async fn require_xpath_element(&self, xpath: &str) -> Result<Element> {
        self.element_by_xpath(xpath)
            .await?
            .ok_or_else(|| Error::ElementNotFound(xpath.to_owned()))
    }

    /// Wait for the first present XPath node in this frame, as in
    /// [`Page::wait_for_xpath_element`]. Uses the polling, timeout, context-change
    /// retry, and cancellation policy of [`Self::wait_for_element`].
    /// Presence does not imply actionability; XPath may select non-element nodes.
    /// This does not reattach a removed frame or follow a process swap.
    pub async fn wait_for_xpath_element(&self, xpath: &str, timeout: Duration) -> Result<Element> {
        let expression = format!(
            "document.evaluate({}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue",
            serde_json::to_string(xpath)?
        );
        self.page
            .wait_for_expression_in_frame(expression, xpath, timeout, Some(self.id.clone()))
            .await
    }

    pub(super) async fn element_for_expression(
        &self,
        expression: String,
    ) -> Result<Option<Element>> {
        self.page
            .element_for_expression_in_frame(expression, Some(self.id.clone()))
            .await
    }
}
