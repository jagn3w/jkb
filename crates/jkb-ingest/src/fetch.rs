//! URL fetching via a headless browser (design D18).
//!
//! A static HTTP fetch would only see the server's initial HTML and miss anything
//! rendered client-side (SPAs, lazy content). So we drive a real headless Chrome:
//! navigate, let JavaScript run, then read the rendered DOM. The resulting HTML is
//! handed to [`crate::adapter::HtmlAdapter`] for text extraction.
//!
//! Requires a Chrome/Chromium binary on the system; [`render_url`] returns an
//! actionable [`Error::Fetch`] if one can't be launched.

use headless_chrome::Browser;

use crate::{Error, Result};

/// Load `url` in a headless browser, wait for it to finish rendering, and return the
/// fully-rendered HTML.
///
/// # Errors
/// Returns [`Error::Fetch`] if the browser can't launch (e.g. no Chrome installed),
/// navigation fails, or the page content can't be read.
pub fn render_url(url: &str) -> Result<String> {
    let browser = Browser::default().map_err(|e| {
        Error::Fetch(format!(
            "could not launch headless Chrome ({e}); install Chrome/Chromium and ensure it is on PATH"
        ))
    })?;
    let tab = browser
        .new_tab()
        .map_err(|e| Error::Fetch(format!("opening a browser tab: {e}")))?;
    tab.navigate_to(url)
        .map_err(|e| Error::Fetch(format!("navigating to {url}: {e}")))?;
    tab.wait_until_navigated()
        .map_err(|e| Error::Fetch(format!("waiting for {url} to load: {e}")))?;
    let html = tab
        .get_content()
        .map_err(|e| Error::Fetch(format!("reading rendered content of {url}: {e}")))?;
    Ok(html)
}
