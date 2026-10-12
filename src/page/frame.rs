use super::{Element, Page, json_value};
use crate::{
    Error, Result,
    cdp::{
        page::{Frame as FrameInfo, FrameId, FrameTree, commands::GetFrameTree},
        runtime::RemoteObject,
    },
};
use serde_json::Value;

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
        json_value(self.evaluate_remote(expression, true).await?)
    }

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
