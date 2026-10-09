use super::Page;
use crate::{
    Error, Result,
    cdp::page::commands::{CaptureScreenshot, PrintToPdf, PrintToPdfTransferMode},
};
use base64::{Engine, engine::general_purpose::STANDARD};

impl Page {
    /// Capture a screenshot as decoded image bytes using the supplied CDP options.
    pub async fn screenshot(&self, options: &CaptureScreenshot) -> Result<Vec<u8>> {
        let result = self.session.execute(options).await?;
        Ok(STANDARD.decode(result.data)?)
    }

    /// Print to PDF bytes. Stream transfer is available through Session::execute.
    pub async fn print_to_pdf(&self, options: &PrintToPdf) -> Result<Vec<u8>> {
        if matches!(
            options.transfer_mode,
            Some(PrintToPdfTransferMode::ReturnAsStream)
        ) {
            return Err(Error::InvalidArgument(
                "print_to_pdf returns bytes; use Session::execute for stream transfer".into(),
            ));
        }
        let result = self.session.execute(options).await?;
        Ok(STANDARD.decode(result.data)?)
    }
}
