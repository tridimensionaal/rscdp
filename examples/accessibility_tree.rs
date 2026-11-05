//! Inspect a form's accessible roles, names and state, then label an unnamed button.
mod common;
use rscdp::{
    Page, Result,
    cdp::accessibility::{AxNode, AxPropertyName, AxValue, commands::GetFullAxTree},
    page::ElementState,
};

fn ax_text(value: Option<&AxValue>) -> Option<&str> {
    value
        .and_then(|value| value.value.as_ref())
        .and_then(|value| value.as_str())
}

fn show_controls(nodes: &[AxNode], phase: &str) {
    println!("{phase}:");
    for node in nodes.iter().filter(|node| !node.ignored) {
        let Some(role @ ("textbox" | "button" | "checkbox")) = ax_text(node.role.as_ref()) else {
            continue;
        };
        // AX fields are optional. Preserve None versus Some("") in the output;
        // Chrome can also preserve whitespace from label content.
        let name = ax_text(node.name.as_ref()).map(str::trim);
        let value = ax_text(node.value.as_ref());
        println!("  role={role}, name={name:?}, value={value:?}");
        if role == "checkbox" {
            let checked = node
                .properties
                .as_ref()
                .and_then(|properties| {
                    properties
                        .iter()
                        .find(|property| property.name == AxPropertyName::Checked)
                })
                .and_then(|property| ax_text(Some(&property.value)));
            // CDP represents checked as a tristate string, not a JSON boolean.
            println!("    checked={checked:?}");
        }
    }
}

async fn inspect_booking(page: &Page) -> Result<()> {
    page.navigate(common::page_url("accessible-booking.html")?)
        .send()
        .await?;
    // One-shot reads do not need Accessibility.enable or stable AX node IDs.
    let before = page.session().execute(&GetFullAxTree::new()).await?;
    show_controls(&before.nodes, "Before labelling the timetable button");

    page.locator("#repair-label").click().await?;
    // Wait for the page's label change before taking another snapshot.
    page.locator("#timetable[aria-label='View timetable']")
        .wait_for(ElementState::Visible)
        .await?;
    let after = page.session().execute(&GetFullAxTree::new()).await?;
    show_controls(&after.nodes, "After labelling the timetable button");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    inspect_booking(browser.page()).await
}
