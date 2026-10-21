//! Browser engine smoke test without a model: iframe click, wait_for,
//! scroll, accessibility tree and tab management.
//!
//! Run with: cargo run -p cua-engine-browser --example browser_smoke

use anyhow::{bail, Context, Result};
use cua_engine_browser::launch;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let dir = std::env::temp_dir().join("cua-browser-smoke");
    std::fs::create_dir_all(&dir)?;
    let page = dir.join("page.html");
    std::fs::write(
        &page,
        r#"<!doctype html><html><body>
<h1>Outer</h1>
<button id="outer" onclick="document.title='outer-ok'">Outer button</button>
<iframe srcdoc="<button id='inner' onclick=&quot;parent.document.title='inner-ok'&quot;>Inner button</button>"></iframe>
</body></html>"#,
    )?;

    let url = format!("file:///{}", page.display().to_string().replace('\\', "/"));
    let mut session = launch(cua_engine_browser::LaunchOptions {
        headed: false,
        start_url: Some(url.clone()),
        ..Default::default()
    })
    .await
    .context("launch")?;

    session.navigate(&url).await?;
    let summary = session.page_summary().await?;
    println!(
        "page: title={:?} ax_lines={}",
        summary.title,
        summary.ax.lines().count()
    );
    if summary.ax.is_empty() {
        bail!("accessibility tree is empty");
    }

    let found = session.wait_for_selector("#inner", 3000).await?;
    println!("wait_for #inner: {found}");
    if !found {
        bail!("#inner not found");
    }

    let clicked = session.click("text=Inner button").await?;
    println!("click inner: {clicked}");
    let title = session.eval("document.title").await?;
    println!("title after iframe click: {title}");
    if title != "inner-ok" {
        bail!("iframe click did not take effect: {title}");
    }

    session.scroll(0, 400).await?;
    println!("scroll: ok");

    let before = session.list_tabs().await?;
    println!("tabs before: {}", before.len());
    session.new_tab("about:blank").await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let opened = session.list_tabs().await?;
    println!("tabs after new: {}", opened.len());
    if opened.len() < before.len() + 1 {
        bail!("new tab not created");
    }

    session.switch_tab(0).await?;
    println!("switched to tab 0: {}", session.page_summary().await?.url);
    session.close_tab(Some(1)).await?;
    let remaining = session.list_tabs().await?;
    println!("tabs after close: {}", remaining.len());
    if remaining.len() != before.len() {
        bail!("tab did not close");
    }

    println!("BROWSER_SMOKE_OK");
    Ok(())
}
