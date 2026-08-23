use std::path::Path;
fn main() {
    let path = std::env::args().nth(1).expect("pkg path");
    let entry_name = std::env::args().nth(2).expect("entry name");
    let pkg = kwe_core::PkgReader::open(Path::new(&path)).expect("open pkg");
    let idx = kwe_core::image_entry(&entry_name, pkg.entries()).expect("find entry");
    let bytes = pkg.read_entry_bounded(idx, 10_000_000).expect("read entry");
    print!("{}", String::from_utf8_lossy(&bytes));
}
