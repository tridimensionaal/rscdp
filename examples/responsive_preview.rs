//! Check a timetable's responsive columns and emulated light/dark appearance.
mod common;
use rustcdp::{
    Error, Page, Result,
    cdp::emulation::{
        MediaFeature,
        commands::{ClearDeviceMetricsOverride, SetDeviceMetricsOverride, SetEmulatedMedia},
    },
};

async fn check_previews(page: &Page) -> Result<()> {
    page.navigate(common::page_url("timetable.html")?)
        .send()
        .await?;
    for (width, columns) in [(1024, 2), (390, 1)] {
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
            let actual: (i64, bool, bool, usize, String, String) = page
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
                .json()
                .await?;
            let dark = scheme == "dark";
            let (background, foreground) = if dark {
                ("rgb(24, 32, 40)", "rgb(240, 244, 248)")
            } else {
                ("rgb(255, 255, 255)", "rgb(32, 40, 48)")
            };
            let expected = (
                width,
                width >= 700,
                dark,
                columns,
                background.into(),
                foreground.into(),
            );
            if actual != expected {
                return Err(Error::InvalidArgument(format!(
                    "unexpected {width}px {scheme} preview: {actual:?}; expected {expected:?}"
                )));
            }
            println!("{width}px {scheme}: {columns} columns, {background} background");
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
    check_previews(browser.page()).await
}
