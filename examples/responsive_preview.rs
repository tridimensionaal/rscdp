//! Preview a timetable's layout and colors at different widths and color schemes.
mod common;
use rscdp::{
    Page, Result,
    cdp::emulation::{
        MediaFeature,
        commands::{ClearDeviceMetricsOverride, SetDeviceMetricsOverride, SetEmulatedMedia},
    },
};

async fn show_previews(page: &Page) -> Result<()> {
    page.navigate(common::page_url("timetable.html")?)
        .send()
        .await?;
    for width in [1024, 390] {
        // Desktop layout at two widths, rather than a complete mobile device profile.
        page.session()
            .execute(&SetDeviceMetricsOverride::new(width, 720, 1.0, false))
            .await?;
        for scheme in ["light", "dark"] {
            page.session()
                .execute(
                    &SetEmulatedMedia::new()
                        .with_features(vec![MediaFeature::new("prefers-color-scheme", scheme)]),
                )
                .await?;
            let (viewport, wide, dark, columns, background, foreground) = page
                .call(
                    r#"() => {
                        const grid = getComputedStyle(document.querySelector('#sessions'));
                        const body = getComputedStyle(document.body);
                        return [innerWidth, matchMedia('(min-width: 700px)').matches,
                            matchMedia('(prefers-color-scheme: dark)').matches,
                            grid.gridTemplateColumns.split(/\s+/).length,
                            body.backgroundColor, body.color];
                    }"#,
                )
                .json::<(i64, bool, bool, usize, String, String)>()
                .await?;
            println!(
                "Requested {width}px / {scheme}: viewport={viewport}px, \
                 wide layout query={wide}, dark scheme query={dark}, \
                 columns={columns}, background={background}, text={foreground}"
            );
        }
    }
    page.session()
        .execute(&ClearDeviceMetricsOverride::new())
        .await?;
    page.session()
        .execute(&SetEmulatedMedia::new().with_media("").with_features(vec![]))
        .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    show_previews(browser.page()).await
}
