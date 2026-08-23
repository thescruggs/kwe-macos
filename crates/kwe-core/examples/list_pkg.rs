use std::path::Path;
fn main() {
    let path = std::env::args().nth(1).expect("path");
    let pkg = kwe_core::PkgReader::open(Path::new(&path)).expect("open pkg");
    for entry in pkg.entries() {
        println!("{}", entry.path);
    }
}
