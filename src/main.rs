fn main() {
    if let Err(error) = nerve_center::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
