use super::{Element, Page};
use crate::{
    Error, Result,
    cdp::{
        page::{Frame as FrameInfo, FrameId, FrameTree, commands::GetFrameTree},
        runtime::RemoteObject,
    },
};
use serde_json::{Value, json};

/// A frame within a CDP target. Clones share the page's resource ownership.
/// Each evaluation resolves the frame's current isolated execution context.
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
    /// Attach to an out-of-process frame target owned by this page, not another tab.
    /// For same-process frames use [`Self::main_frame`] or [`Self::frames`].
    /// After target selection, an owned task retains the parent Page through attach
    /// and domain setup, even if the caller cancels or its handle timeout expires.
    /// The child restores that handle timeout. With `None`, the caller waits for
    /// setup while the independent attachment and domain-transition limits remain.
    /// The returned Page shares any fallback-context ownership with its parent.
    pub async fn frame(&self, index: usize) -> Result<Page> {
        let connection = self.session.connection();
        let targets = connection
            .execute(&crate::cdp::target::commands::GetTargets::default())
            .await?;
        let mut matched = 0;
        for target in targets.target_infos.into_iter().filter(|target| {
            target.type_ == "iframe" && target.target_id != *self.session.target_id()
        }) {
            match self
                .session
                .execute_raw("DOM.getFrameOwner", json!({"frameId":target.target_id}))
                .await
            {
                Ok(_) => {
                    if matched == index {
                        let parent = self.clone();
                        let timeout = connection.command_timeout();
                        let task = connection.runtime().spawn(async move {
                            let session = parent
                                .session
                                .connection()
                                .clone()
                                .with_command_timeout(None)
                                .attach(target.target_id)
                                .await?
                                .with_command_timeout(timeout);
                            let mut page =
                                Page::from_session(session, parent.context.clone()).await?;
                            page.context_lease = parent.context_lease.clone();
                            drop(parent);
                            Ok(page)
                        });
                        let joined = if let Some(timeout) = timeout {
                            tokio::time::timeout(timeout, task)
                                .await
                                .map_err(|_| Error::Timeout("attaching frame".into()))?
                        } else {
                            task.await
                        };
                        return joined?;
                    }
                    matched += 1;
                }
                Err(Error::Protocol(error))
                    if error.code == -32000
                        && (error.message == "Frame with the given id was not found."
                            || error.message
                                == "Frame with the given id does not belong to the target.") => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::InvalidArgument(
            "frame target index out of bounds".into(),
        ))
    }

    pub async fn main_frame(&self) -> Result<Frame> {
        let tree = self.session.execute(&GetFrameTree::new()).await?.frame_tree;
        Ok(Frame {
            page: self.clone(),
            id: tree.frame.id,
        })
    }

    /// Snapshot all frames exposed by this target's frame tree, including its root.
    /// OOPIF targets are separate: attach with `Page::frame`, then use `main_frame`.
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
            .ok_or_else(|| Error::InvalidArgument("frame is no longer in this target".into()))
    }

    /// Snapshot direct children belonging to this target's frame tree.
    pub async fn children(&self) -> Result<Vec<Frame>> {
        let tree = self
            .page
            .session
            .execute(&GetFrameTree::new())
            .await?
            .frame_tree;
        let tree = find(&tree, &self.id)
            .ok_or_else(|| Error::InvalidArgument("frame is no longer in this target".into()))?;
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

    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.page
            .json_value(self.evaluate_remote(expression, true).await?)
    }

    /// Evaluate in this frame, with the same remote object ownership as
    /// [`Page::evaluate_remote`].
    pub async fn evaluate_remote(
        &self,
        expression: &str,
        return_by_value: bool,
    ) -> Result<RemoteObject> {
        let context = self.page.isolated_context_for(&self.id).await?;
        self.page
            .evaluate_with_context(context.unique, expression, return_by_value)
            .await
    }

    pub async fn html(&self) -> Result<String> {
        serde_json::from_value(self.evaluate("document.documentElement.outerHTML").await?)
            .map_err(Into::into)
    }

    /// Find a node in this frame with the owned acquisition and cleanup policy
    /// of [`Page::element_by_selector`].
    pub async fn element_by_selector(&self, selector: &str) -> Result<Option<Element>> {
        let selector = serde_json::to_string(selector)?;
        self.element_for_expression(format!("document.querySelector({selector})"))
            .await
    }

    /// Find an XPath node in this frame with the owned acquisition and cleanup
    /// policy of [`Page::element_by_xpath`].
    pub async fn element_by_xpath(&self, xpath: &str) -> Result<Option<Element>> {
        let xpath = serde_json::to_string(xpath)?;
        self.element_for_expression(format!("document.evaluate({xpath}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue")).await
    }

    async fn element_for_expression(&self, expression: String) -> Result<Option<Element>> {
        self.page
            .element_for_expression_in_frame(expression, Some(self.id.clone()))
            .await
    }
}
