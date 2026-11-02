//! Inspect a form's accessible roles, names and state, then repair an unnamed button.
mod common;
use rustcdp::{
    Error, Page, Result,
    cdp::accessibility::{AxNode, AxPropertyName, AxValue, commands::GetFullAxTree},
    page::ElementState,
};

fn ax_text(value: Option<&AxValue>) -> Option<&str> {
    value
        .and_then(|value| value.value.as_ref())
        .and_then(|value| value.as_str())
}

fn find_control<'a>(nodes: &'a [AxNode], role: &str, name: &str) -> Result<&'a AxNode> {
    nodes
        .iter()
        .find(|node| {
            !node.ignored
                && ax_text(node.role.as_ref()) == Some(role)
                // Chrome can preserve leading/trailing whitespace from label content.
                && ax_text(node.name.as_ref()).map(str::trim) == Some(name)
        })
        .ok_or_else(|| Error::InvalidArgument(format!("missing accessible {role} named {name:?}")))
}

fn check_form(nodes: &[AxNode]) -> Result<()> {
    let name = find_control(nodes, "textbox", "Name")?;
    if ax_text(name.value.as_ref()) != Some("Lucía") {
        return Err(Error::InvalidArgument(
            "unexpected accessible textbox value".into(),
        ));
    }
    find_control(nodes, "button", "Book workshop")?;
    let receipts = find_control(nodes, "checkbox", "Email booking receipt")?;
    let checked = receipts
        .properties
        .as_ref()
        .and_then(|properties| {
            properties
                .iter()
                .find(|property| property.name == AxPropertyName::Checked)
        })
        .and_then(|property| ax_text(Some(&property.value)));
    // CDP represents the checkbox's tristate value as a string, not a JSON boolean.
    if checked != Some("true") {
        return Err(Error::InvalidArgument(format!(
            "unexpected accessible checkbox state: {checked:?}"
        )));
    }
    Ok(())
}

async fn inspect_booking(page: &Page) -> Result<()> {
    page.navigate(common::page_url("accessible-booking.html")?)
        .send()
        .await?;
    // One-shot reads do not need Accessibility.enable or stable AX node IDs.
    let before = page.session().execute(&GetFullAxTree::new()).await?;
    check_form(&before.nodes)?;
    find_control(&before.nodes, "button", "")?;
    if before.nodes.iter().any(|node| {
        !node.ignored
            && ax_text(node.role.as_ref()) == Some("button")
            && ax_text(node.name.as_ref()).map(str::trim) == Some("View timetable")
    }) {
        return Err(Error::InvalidArgument(
            "timetable button was already labelled".into(),
        ));
    }
    page.locator("#repair-label").click().await?;
    page.locator("#timetable[aria-label='View timetable']")
        .wait_for(ElementState::Visible)
        .await?;
    let after = page.session().execute(&GetFullAxTree::new()).await?;
    check_form(&after.nodes)?;
    find_control(&after.nodes, "button", "View timetable")?;
    if after.nodes.iter().any(|node| {
        !node.ignored
            && ax_text(node.role.as_ref()) == Some("button")
            && ax_text(node.name.as_ref()).is_none_or(|name| name.trim().is_empty())
    }) {
        return Err(Error::InvalidArgument(
            "an accessible button still lacks a name".into(),
        ));
    }
    println!(
        "Verified Name textbox, Book workshop button and checked receipt checkbox; repaired View timetable label."
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    inspect_booking(browser.page()).await
}
