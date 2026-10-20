use super::Page;
use crate::{Error, Result, cdp::page::commands::PrintToPdf};
use base64::{Engine, engine::general_purpose::STANDARD};

/// Configure PDF output, then call [`Self::generate`] to return its bytes.
/// Defaults to portrait US Letter paper, no background graphics, and all pages.
/// For headers, footers and other protocol options, use [`Page::print_to_pdf`].
#[must_use = "call generate().await to create the PDF"]
pub struct PdfBuilder<'a> {
    page: &'a Page,
    options: PrintToPdf,
}

impl PdfBuilder<'_> {
    /// Print in landscape orientation. Defaults to false.
    pub fn landscape(mut self, enabled: bool) -> Self {
        self.options.landscape = Some(enabled);
        self
    }

    /// Include background graphics. Defaults to false.
    pub fn print_background(mut self, enabled: bool) -> Self {
        self.options.print_background = Some(enabled);
        self
    }

    /// Use the CSS `@page` size instead of scaling content to the paper size.
    pub fn prefer_css_page_size(mut self, enabled: bool) -> Self {
        self.options.prefer_css_page_size = Some(enabled);
        self
    }

    /// Set paper width and height in inches. Both must be positive and finite.
    /// Defaults to 8.5 × 11 inches (US Letter).
    pub fn paper_size_inches(mut self, width: f64, height: f64) -> Self {
        self.options.paper_width = Some(width);
        self.options.paper_height = Some(height);
        self
    }

    /// Scale the rendered page, from 0.1 to 2.0. Defaults to 1.0.
    pub fn scale(mut self, scale: f64) -> Self {
        self.options.scale = Some(scale);
        self
    }

    /// Select one-based page ranges, for example `"1-3, 5"`.
    /// Empty means all pages. Chrome validates the ranges when generating.
    pub fn page_ranges(mut self, ranges: impl Into<String>) -> Self {
        self.options.page_ranges = Some(ranges.into());
        self
    }

    /// Print the page and return decoded PDF bytes.
    pub async fn generate(self) -> Result<Vec<u8>> {
        if self
            .options
            .scale
            .is_some_and(|scale| !scale.is_finite() || !(0.1..=2.0).contains(&scale))
        {
            return Err(Error::InvalidArgument(
                "PDF scale must be between 0.1 and 2.0".into(),
            ));
        }
        if [self.options.paper_width, self.options.paper_height]
            .into_iter()
            .flatten()
            .any(|n| !n.is_finite() || n <= 0.0)
        {
            return Err(Error::InvalidArgument(
                "PDF paper dimensions must be positive and finite".into(),
            ));
        }
        self.page.print_to_pdf(&self.options).await
    }
}

impl Page {
    /// Configure a PDF export. Call `generate().await` to return PDF bytes.
    ///
    /// ```no_run
    /// # async fn example(page: &rustcdp::Page) -> rustcdp::Result<()> {
    /// let pdf = page.pdf().print_background(true).generate().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn pdf(&self) -> PdfBuilder<'_> {
        PdfBuilder {
            page: self,
            options: PrintToPdf::new(),
        }
    }

    /// Print to PDF bytes using all available CDP options.
    /// Prefer [`Self::pdf`] for common settings. Stream transfer is available
    /// through [`crate::Session::execute`].
    pub async fn print_to_pdf(&self, options: &PrintToPdf) -> Result<Vec<u8>> {
        if options
            .transfer_mode
            .as_ref()
            .is_some_and(|mode| mode.as_str() != "ReturnAsBase64")
        {
            return Err(Error::InvalidArgument(
                "print_to_pdf requires base64 transfer; use Session::execute for other modes"
                    .into(),
            ));
        }
        let result = self.session.execute(options).await?;
        Ok(STANDARD.decode(result.data)?)
    }
}
