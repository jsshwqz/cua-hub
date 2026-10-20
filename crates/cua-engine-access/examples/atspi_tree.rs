// Print the accessibility tree. Linux: AT-SPI via D-Bus; Windows: UIA.
fn main() {
    match cua_engine_access::snapshot(120, 6) {
        Ok(snap) => {
            print!("{}", snap.to_text());
            eprintln!("\n{} nodes, truncated={}", snap.nodes.len(), snap.truncated);
        }
        Err(e) => {
            eprintln!("snapshot failed: {e}");
            std::process::exit(1);
        }
    }
}
