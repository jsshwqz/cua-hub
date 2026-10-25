// Find a control by accessible name and print its rectangle (Linux: AT-SPI).
fn main() {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Click Me".to_string());
    let target = cua_engine_access::target::Target::by_name(&name);
    match cua_engine_access::find_target(&target, 400, 8) {
        Ok(node) => {
            let (x, y, w, h) = node.rect;
            println!(
                "{} \"{}\" at ({x},{y},{w},{h}) center=({},{})",
                node.control_type,
                node.name,
                x + w / 2,
                y + h / 2
            );
        }
        Err(e) => {
            eprintln!("find failed: {e}");
            std::process::exit(1);
        }
    }
}
