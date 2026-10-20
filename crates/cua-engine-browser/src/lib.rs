//! Browser automation over the Chrome DevTools Protocol.
//!
//! Launches a local Chrome/Edge with an isolated profile and remote debugging,
//! then drives it with a JSON action loop powered by any OpenAI-compatible
//! vision model. Browser actions are intentionally separate from the desktop
//! [`cua_core::Action`] space: DOM selectors and page evaluation do not map
//! cleanly onto mouse coordinates.

mod agent;
mod cdp;

pub use agent::{
    parse_browser_action, run_task, BrowserAction, BrowserOutcome, BrowserReport, BrowserRunOptions,
};
pub use cdp::{launch, BrowserSession};
