// Temporary coordinate-space check (not part of the crate API).
use enigo::{Coordinate, Enigo, Mouse, Settings};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut e = Enigo::new(&Settings::default())?;
    for (x, y) in [(10, 10), (500, 300), (960, 540)] {
        e.move_mouse(x, y, Coordinate::Abs)?;
        std::thread::sleep(std::time::Duration::from_millis(120));
        let got = e.location()?;
        println!("requested ({x},{y}) -> reported {got:?}");
    }
    Ok(())
}
