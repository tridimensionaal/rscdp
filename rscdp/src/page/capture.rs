use super::Page;
use crate::{Error, Result, cdp::page::commands::CaptureScreenshot};
use base64::{Engine, engine::general_purpose::STANDARD};

pub use crate::cdp::page::commands::CaptureScreenshotFormat as ScreenshotFormat;

/// A screenshot request. Defaults to a PNG of the visible viewport.
///
/// Configure the request, then call [`Self::capture`] to return image bytes.
/// Full-page capture measures the document immediately before capture; a page
/// that changes layout during these commands can still change the captured area.
#[must_use = "call capture().await to take the screenshot"]
pub struct ScreenshotBuilder<'a> {
    page: &'a Page,
    options: CaptureScreenshot,
    full_page: bool,
}

impl ScreenshotBuilder<'_> {
    /// Capture the entire document instead of the visible viewport.
    /// Cannot be combined with an explicit clip.
    pub fn full_page(mut self, enabled: bool) -> Self {
        self.full_page = enabled;
        self
    }

    /// Image encoding. Defaults to PNG.
    pub fn format(mut self, format: ScreenshotFormat) -> Self {
        self.options.format = Some(format);
        self
    }

    /// JPEG compression quality, from 0 to 100. Requires JPEG format.
    pub fn quality(mut self, quality: u8) -> Self {
        self.options.quality = Some(i64::from(quality));
        self
    }

    /// Capture a region in CSS pixels, including portions outside the viewport.
    /// Width, height and scale must be positive and finite; x/y must be finite.
    /// Cannot be combined with full-page capture.
    pub fn clip(mut self, clip: crate::cdp::page::Viewport) -> Self {
        self.options.clip = Some(clip);
        self
    }

    /// Send the request and return decoded image bytes.
    pub async fn capture(mut self) -> Result<Vec<u8>> {
        if self.full_page && self.options.clip.is_some() {
            return Err(Error::InvalidArgument(
                "full_page and clip cannot be combined".into(),
            ));
        }
        if let Some(quality) = self.options.quality
            && (quality > 100
                || self
                    .options
                    .format
                    .as_ref()
                    .is_none_or(|format| format.as_str() != "jpeg"))
        {
            return Err(Error::InvalidArgument(
                "screenshot quality requires JPEG format and a value from 0 to 100".into(),
            ));
        }
        if let Some(clip) = &self.options.clip
            && (!clip.x.is_finite()
                || !clip.y.is_finite()
                || [clip.width, clip.height, clip.scale]
                    .iter()
                    .any(|n| !n.is_finite() || *n <= 0.0))
        {
            return Err(Error::InvalidArgument("screenshot clip requires finite coordinates and positive finite dimensions and scale".into()));
        }
        if self.full_page {
            let size = self
                .page
                .session
                .execute(&crate::cdp::page::commands::GetLayoutMetrics::new())
                .await?
                .css_content_size;
            self.options.clip = Some(crate::cdp::page::Viewport {
                x: size.x,
                y: size.y,
                width: size.width,
                height: size.height,
                scale: 1.0,
            });
        }
        self.options.capture_beyond_viewport = Some(self.options.clip.is_some());
        self.page.screenshot_with_options(&self.options).await
    }
}

impl Page {
    /// Configure a screenshot. Call `capture().await` to return image bytes.
    ///
    /// ```no_run
    /// # async fn example(page: &rscdp::Page) -> rscdp::Result<()> {
    /// let png = page.screenshot().full_page(true).capture().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn screenshot(&self) -> ScreenshotBuilder<'_> {
        ScreenshotBuilder {
            page: self,
            options: CaptureScreenshot::new(),
            full_page: false,
        }
    }

    /// Capture using all available CDP options and return decoded image bytes.
    /// Prefer [`Self::screenshot`] for viewport, full-page and clipped captures.
    pub async fn screenshot_with_options(&self, options: &CaptureScreenshot) -> Result<Vec<u8>> {
        let result = self.session.execute(options).await?;
        Ok(STANDARD.decode(result.data)?)
    }
}
