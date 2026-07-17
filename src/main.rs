mod detector;

use detector::{windows::WindowsDetector, MeetingDetector};
use std::{thread, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let det = WindowsDetector::new()?;
    println!("Слежу за mic-сессиями. Ctrl+C для выхода.");
    loop {
        match det.poll() {
            Ok(sessions) if sessions.is_empty() => println!("— тихо —"),
            Ok(sessions) => {
                for s in sessions {
                    println!("АКТИВНА: {} (pid {})", s.process_name, s.pid);
                }
            }
            Err(e) => eprintln!("ошибка: {e}"),
        }
        thread::sleep(Duration::from_secs(2));
    }
}
