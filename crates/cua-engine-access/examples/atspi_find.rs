// Find a control by accessible name and print its rectangle (Linux: AT-SPI).
fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Click Me".to_string());
    match cua_engine_access::find(&name, 400, 8) {
        Ok(Some(node)) => {
            let (x, y, w, h) = node.rect;
            println!(
                "{} \"{}\" at ({x},{y},{w},{h}) center=({},{})",
                node.control_type,
                node.name,
                x + w / 2,
                y + h / 2
            );
        }
        Ok(None) => {
            eprintln!("no control matching {name:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("find failed: {e}");
            std::process::exit(1);
        }
    }
}
